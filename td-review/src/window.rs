//! The integrator's window: td-ui's widget window shows the frames the state
//! machine lays out and hands it the inputs made against them.
//!
//! The state machine runs on a thread of its own. Fetching, landing, pushing
//! and deleting are git processes that take as long as the network does, and
//! the panes say what they are about to do before each one starts (`redraw`
//! in `app.rs`); a window that ran them on its own thread could neither show
//! that frame nor answer the compositor until they ended. So the window
//! thread owns the Wayland connection and paints whatever frame arrived last,
//! and the worker owns the `App`, its git, and every decision.
//!
//! What crosses between them is what a terminal used to carry. Inputs go to
//! the worker in the order they were made, each stamped with a frame the
//! window had shown before the input reached it; frames come back with the
//! count of inputs taken. A confirmation must be answered by an input made
//! after it appeared, so when the state machine drains its typeahead the
//! worker drops every input stamped with an earlier frame than the one it
//! drew last.
//!
//! The stamp is conservative twice over. An input is read when the window
//! reads its socket, which can be after a paint the keystroke came before:
//! so it is stamped with the newest frame painted at least `DWELL_MS` before
//! it was read, a span past the window's own read-and-paint latency and
//! short of anybody's reaction to a new prompt. And a frame counts as shown
//! only once it was painted whole: on a surface too small for its rows, the
//! prompt bar is clipped away, and nothing could have seen it.
//!
//! The review's preview is not rows. A frame hands it whole to the window,
//! which shows it in td-ui's document pane, td-news's and td-mail's reader:
//! the pane scrolls it, selects in it by drag, word and line, and the window
//! copies the selection to the clipboard on `C-c`. Those inputs stay on the
//! window thread; they decide nothing, so no stamp guards them.
//!
//! So does `?`, which opens the window's key list (`keys.rs`) as `F1` does,
//! whatever pane or prompt is up: the worker never sees it. A branch name
//! cannot hold `?`, so the filter loses nothing it could match, and a
//! confirmation is left up rather than answered.

use std::collections::VecDeque;
use std::io;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use td_ui::chrome::SELECTED_ROW;
use td_ui::editor::{self, Controller, Outcome};
use td_ui::editor_clipboard::{self, Snapshot};
use td_ui::editor_error::Error as PaneError;
use td_ui::editor_model::TabId;
use td_ui::pointer::DoubleClick;
use td_ui::raster::{
    self, Composition, Draw, GlyphStyle, Primitive, Raster, Rect, Scrollbar, Surface, Weight,
    ACCENT, CHROME, INK, LINE_NUMBER, MISSPELLED, PAPER, SELECTED, SUCCESS, WARNING,
};
#[cfg(test)]
use td_ui::window::NoClipboard;
use td_ui::window::{Clipboard, Flow, Handler, Input, PointerPhase};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

use crate::app::{self, App};
use crate::keys;
use crate::view::{self, Document, Frame, Key, Pane, Style, Ui, CYAN, GREEN, MAGENTA, RED, YELLOW};

/// A row's height at scale one: a cell with a pixel of leading above and
/// below, so the bars' fills do not touch the glyphs of the rows beside them.
const LINE: usize = CELL_HEIGHT + 2;
const LEADING: usize = 1;
/// The ground left of the text.
const MARGIN: usize = CELL_WIDTH / 2;
/// The column right of the text the scrollbar is drawn in, and its track.
const GUTTER: usize = 12;
const TRACK: usize = 8;
/// The smallest grid the panes are laid out on, whatever the surface: the
/// list needs its title, header and status rows and one row between them.
const MIN_ROWS: usize = 4;
const MIN_COLS: usize = 20;

/// How long a turn may wait: briefly while an input is on its way to the
/// worker, longer while it works on one, and the window's own idle pace
/// once it waits for the next.
const AWAITED_MS: u64 = 5;
const WORKING_MS: u64 = 25;
const IDLE_MS: u64 = 100;

/// How long a frame must have been on screen before an input read is taken
/// to have been made against it.
const DWELL_MS: u64 = 80;
/// Frames the stamp remembers: more than are painted within `DWELL_MS`.
const SHOWN_KEPT: usize = 32;

/// What the status row says once the window was asked to close.
const CLOSING: &str = " closing once the running git command finishes…";

/// The grid `surface` holds, rows then columns.
pub fn grid(surface: Surface) -> (usize, usize) {
    let s = surface.scale.value();
    let rows = surface.height / (LINE * s);
    let cols = surface.width.saturating_sub((MARGIN + GUTTER) * s) / (CELL_WIDTH * s);
    (rows.max(MIN_ROWS), cols.max(MIN_COLS))
}

/// Whether `frame` is painted whole on `surface`: every row it holds and
/// every cell of its width inside the surface. One that is not may have lost
/// its prompt bar to the clip.
fn fits(frame: &Frame, surface: Surface) -> bool {
    let s = surface.scale.value();
    frame.lines().len() * LINE * s <= surface.height
        && (frame.cols * CELL_WIDTH + MARGIN + GUTTER) * s <= surface.width
}

/// Where `pane` lies on `surface`: its rows across the whole width, cut short
/// at the surface's foot; none when no row of it is on the surface.
fn pane_rect(pane: &Pane, surface: Surface) -> Option<Rect> {
    let s = surface.scale.value();
    let y = pane.row.saturating_mul(LINE * s);
    let height = pane
        .rows
        .saturating_mul(LINE * s)
        .min(surface.height.saturating_sub(y));
    if height == 0 || surface.width == 0 {
        return None;
    }
    Some(Rect {
        x: 0,
        y: i64::try_from(y).ok()?,
        width: u32::try_from(surface.width).ok()?,
        height: u32::try_from(height).ok()?,
    })
}

/// How far a reading key scrolls a pane of `page` rows: a row, a page, half
/// one, or to an end; none for any other key.
fn scroll_of(key: Key, page: usize) -> Option<isize> {
    let page = isize::try_from(page.max(1)).unwrap_or(isize::MAX);
    Some(match key {
        Key::Char('j') | Key::Down | Key::Enter => 1,
        Key::Char('k') | Key::Up => -1,
        Key::Char(' ') | Key::PageDown | Key::Ctrl('f') => page,
        Key::Char('b') | Key::PageUp | Key::Ctrl('b') => -page,
        Key::Ctrl('d') => page / 2,
        Key::Ctrl('u') => -(page / 2),
        Key::Char('g') | Key::Home => -isize::MAX,
        Key::Char('G') | Key::End => isize::MAX,
        _ => return None,
    })
}

/// The ink a palette slot is drawn in on the toolkit's paper. The slots name
/// what a line means (a failure, a success, a warning, a heading), so each
/// is the toolkit's ink for that meaning, which its themes recolour.
fn ink(code: u8) -> u32 {
    match code {
        RED => MISSPELLED,
        GREEN => SUCCESS,
        YELLOW => WARNING,
        MAGENTA => ACCENT,
        CYAN => SELECTED,
        _ => INK,
    }
}

/// A row's band, if it fills one, and how its glyphs are drawn. An inverted
/// row is a bar: in its slot's colour with paper ink, or, with no slot, the
/// toolkit's selected-row ground (the list's cursor) or chrome (a dim note).
/// Bold has no face of its own in the raster; the slot carries it.
fn paint_of(style: Style) -> (Option<u32>, GlyphStyle) {
    let (band, ink) = match (style.invert, style.fg) {
        (true, Some(code)) => (Some(ink(code)), PAPER),
        (true, None) if style.dim => (Some(CHROME), LINE_NUMBER),
        (true, None) => (Some(SELECTED_ROW & 0x00ff_ffff), INK),
        (false, _) if style.dim => (None, LINE_NUMBER),
        (false, Some(code)) => (None, ink(code)),
        (false, None) => (None, INK),
    };
    (
        band,
        GlyphStyle {
            ink,
            background: band.unwrap_or(PAPER),
            weight: Weight::Medium,
        },
    )
}

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(clip) = rect.intersection(damage) {
        sink(Draw {
            clip,
            primitive: Primitive::Fill { rect, color },
        });
    }
}

/// A frame laid out over a surface: what the window paints and a test reads.
pub struct Painting<'a> {
    pub surface: Surface,
    pub frame: Option<&'a Frame>,
    /// Painted over the frame's last row, as a bar of its own.
    pub notice: Option<&'a str>,
    /// The document pane, painted over the rows the frame keeps for it.
    pub pane: Option<&'a dyn Composition>,
}

impl Painting<'_> {
    fn row(
        &self,
        index: usize,
        text: &str,
        style: Style,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let scale = self.surface.scale;
        let s = scale.value();
        let row = Rect {
            x: 0,
            y: (index * LINE * s) as i64,
            width: self.surface.width as u32,
            height: (LINE * s) as u32,
        };
        let (band, glyphs) = paint_of(style);
        if let Some(color) = band {
            fill(row, color, damage, sink);
        }
        let text_area = Rect {
            x: (MARGIN * s) as i64,
            width: self.surface.width.saturating_sub((MARGIN + GUTTER) * s) as u32,
            ..row
        };
        raster::text_run(
            scale,
            text.chars(),
            (text_area.x, row.y + (LEADING * s) as i64),
            text_area,
            glyphs,
            damage,
            sink,
        );
    }
}

impl Composition for Painting<'_> {
    fn surface(&self) -> Surface {
        self.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let bounds = self.surface.bounds();
        let Some(damage) = damage.intersection(bounds) else {
            return;
        };
        fill(bounds, PAPER, damage, sink);
        let Some(frame) = self.frame else {
            return;
        };
        let scale = self.surface.scale;
        let s = scale.value();
        for (i, line) in frame.lines().iter().enumerate() {
            if i * LINE * s >= self.surface.height {
                break;
            }
            self.row(i, &line.text, line.style, damage, sink);
        }
        if let Some(pane) = self.pane {
            pane.emit(damage, sink);
        }
        if let Some(scroll) = frame
            .scroll()
            .filter(|s| s.visible > 0 && s.total > s.visible)
        {
            let track = Rect {
                x: self.surface.width.saturating_sub((GUTTER + TRACK) / 2 * s) as i64,
                y: (scroll.row * LINE * s) as i64,
                width: (TRACK * s) as u32,
                height: (scroll.visible * LINE * s) as u32,
            };
            let bar = Scrollbar::new(
                track,
                scroll.visible,
                scroll.total,
                scroll.first,
                scale,
                false,
            );
            fill(track, CHROME, damage, sink);
            fill(bar.thumb, LINE_NUMBER, damage, sink);
        }
        // On the frame's last row, or the surface's when a shrink the worker
        // has not laid out again yet has cut the frame short.
        let shows = self.surface.height / (LINE * s);
        let last = frame.lines().len().min(shows).checked_sub(1);
        if let (Some(notice), Some(last)) = (self.notice, last) {
            let (text, _) = view::sanitize(notice, frame.cols);
            self.row(last, &text, Style::bar(YELLOW), damage, sink);
        }
    }
}

/// An input for the worker, stamped with a frame shown before it was read.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Key {
        key: Key,
        seen: u64,
    },
    /// A press on the list row that showed `refname`.
    Pick {
        refname: String,
        double: bool,
        seen: u64,
    },
    Wheel {
        rows: isize,
        seen: u64,
    },
    /// The grid changed: lay the frame out again.
    Redraw,
    /// The compositor asked the window to close.
    Close,
}

/// What the worker tells the window. Each carries how many inputs it has
/// taken off the channel so far, kept or dropped.
enum Message {
    Frame {
        generation: u64,
        frame: Frame,
        taken: u64,
    },
    /// The worker has taken an input and is acting on it.
    Taken { taken: u64 },
    /// Every input sent has been taken and the worker waits for the next.
    Idle { taken: u64 },
}

/// The grid the window last laid out for, which the worker lays frames on.
struct Grid {
    rows: AtomicUsize,
    cols: AtomicUsize,
}

impl Grid {
    fn new((rows, cols): (usize, usize)) -> Grid {
        Grid {
            rows: AtomicUsize::new(rows),
            cols: AtomicUsize::new(cols),
        }
    }

    fn set(&self, (rows, cols): (usize, usize)) {
        self.rows.store(rows, Ordering::Relaxed);
        self.cols.store(cols, Ordering::Relaxed);
    }

    fn get(&self) -> (usize, usize) {
        (
            self.rows.load(Ordering::Relaxed),
            self.cols.load(Ordering::Relaxed),
        )
    }
}

/// The worker's side: the state machine's `Ui`.
struct Link {
    events: Receiver<Event>,
    messages: Sender<Message>,
    grid: Arc<Grid>,
    /// The frame drawn last.
    generation: u64,
    /// Inputs stamped with an earlier frame than this are dropped.
    floor: u64,
    taken: u64,
}

impl Link {
    fn new(events: Receiver<Event>, messages: Sender<Message>, grid: Arc<Grid>) -> Link {
        Link {
            events,
            messages,
            grid,
            generation: 0,
            floor: 0,
            taken: 0,
        }
    }

    /// The next input made against a frame no older than the floor, or none
    /// once the window has gone.
    fn next(&mut self) -> Option<Event> {
        loop {
            let event = match self.events.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty) => {
                    // Said before blocking, so the window stops polling at the
                    // pace of an input in flight. A window that has gone is
                    // seen by the receive.
                    let _ = self.messages.send(Message::Idle { taken: self.taken });
                    self.events.recv().ok()?
                }
                Err(TryRecvError::Disconnected) => return None,
            };
            self.taken = self.taken.saturating_add(1);
            match event {
                Event::Key { seen, .. } | Event::Pick { seen, .. } | Event::Wheel { seen, .. }
                    if seen < self.floor => {}
                event => {
                    let _ = self.messages.send(Message::Taken { taken: self.taken });
                    return Some(event);
                }
            }
        }
    }
}

impl Ui for Link {
    fn size(&self) -> (usize, usize) {
        self.grid.get()
    }

    fn draw(&mut self, frame: Frame) -> io::Result<()> {
        self.generation = self.generation.saturating_add(1);
        self.messages
            .send(Message::Frame {
                generation: self.generation,
                frame,
                taken: self.taken,
            })
            .map_err(|_| io::Error::other("the window has closed"))
    }

    fn drain_input(&mut self) -> io::Result<()> {
        self.floor = self.generation;
        Ok(())
    }
}

/// The worker: draw, settle, take the next input, act on it — the loop the
/// terminal ran, over the window's inputs.
fn serve(mut app: App, mut link: Link) -> io::Result<()> {
    loop {
        let (rows, cols) = link.size();
        link.draw(app.render(rows, cols))?;
        // The prompt is on screen now: drop anything made while the last
        // command ran, before taking the answer.
        app.settle_prompt(&mut link)?;
        let Some(event) = link.next() else {
            return Ok(());
        };
        match event {
            Event::Key { key, .. } => {
                if matches!(app.feed(vec![key], &mut link)?, app::Flow::Quit) {
                    return Ok(());
                }
            }
            Event::Pick {
                refname, double, ..
            } => app.pick(&refname, double, link.size().0)?,
            Event::Wheel { rows, .. } => {
                let (height, cols) = link.size();
                app.wheel(rows, height, cols);
            }
            Event::Redraw => {}
            // As `q` does, and from a prompt as leaving it does: a confirmation
            // still up is answered no, an unfinished landing left as it is.
            Event::Close => return Ok(()),
        }
    }
}

/// A frame painted whole: its generation, when, and its list rows' branches.
#[derive(Debug)]
struct Shown {
    generation: u64,
    at: Instant,
    picks: Vec<(usize, String)>,
}

/// The document the pane holds: which one, its tab, and the inks its styled
/// lines are drawn in.
struct Held {
    id: u64,
    tab: Option<TabId>,
    inks: Vec<(Range<usize>, u32)>,
}

/// The window thread's side: the widget window's handler.
struct Session {
    title: String,
    events: Sender<Event>,
    messages: Receiver<Message>,
    grid: Arc<Grid>,
    /// The frame received last and not yet painted, with its generation.
    pending: Option<(u64, Frame)>,
    /// The frame on screen, with its generation.
    shown: Option<(u64, Frame)>,
    /// Generations painted whole, oldest first: when, and the branch each
    /// list row showed.
    painted: VecDeque<Shown>,
    dirty: bool,
    sent: u64,
    taken: u64,
    idle: bool,
    closing: bool,
    scale: usize,
    epoch: Instant,
    clicks: DoubleClick<usize>,
    /// td-ui's document pane, for the review's preview.
    pane: Controller,
    held: Option<Held>,
    /// A press in the pane is held: its drag and release are the pane's.
    dragging: bool,
    /// The surface painted last, which the pane is placed on.
    surface: Option<Surface>,
    /// The window's clock at its last turn, the pane's ticks.
    clock: u64,
    /// What a copy came to, over the status row until the next input or
    /// the next frame, so it never covers a prompt bar a frame raised.
    note: Option<String>,
    /// `?` was pressed: the window's key list opens after this input.
    show_keys: bool,
}

impl Session {
    fn new(
        title: String,
        events: Sender<Event>,
        messages: Receiver<Message>,
        grid: Arc<Grid>,
    ) -> Result<Self, String> {
        Ok(Session {
            title,
            events,
            messages,
            grid,
            pending: None,
            shown: None,
            painted: VecDeque::with_capacity(SHOWN_KEPT),
            dirty: true,
            sent: 0,
            taken: 0,
            idle: false,
            closing: false,
            scale: 1,
            epoch: Instant::now(),
            clicks: DoubleClick::default(),
            pane: Controller::pane().map_err(|e| format!("document pane: {e}"))?,
            held: None,
            dragging: false,
            surface: None,
            clock: 0,
            note: None,
            show_keys: false,
        })
    }

    /// The pane the frame on screen shows, if it shows one.
    fn region(&self) -> Option<&Pane> {
        self.shown.as_ref().and_then(|(_, frame)| frame.shows())
    }

    /// The pane's tab and revision while the frame on screen shows the
    /// document it holds.
    fn showing(&self) -> Option<(TabId, u64)> {
        let held = self.held.as_ref()?;
        if self.region()?.document.id != held.id {
            return None;
        }
        let tab = held.tab?;
        let revision = self.pane.editor().document(tab).ok()?.revision();
        Some((tab, revision))
    }

    /// Hands the pane `event`; a change while it is on screen repaints. A
    /// refusal is nothing: the pane is as it was.
    fn pane_event(&mut self, event: editor::Event<'_>) -> Outcome {
        let outcome = self.pane.dispatch(event).unwrap_or(Outcome::Ignored);
        if outcome == Outcome::Changed && self.region().is_some() {
            self.dirty = true;
        }
        outcome
    }

    /// Places the pane where the frame on screen shows it on `surface`, and
    /// has it hold that frame's document, loaded afresh at its top when it
    /// is a new one.
    fn sync_pane(&mut self, surface: Surface) {
        let Some(region) = self.region().cloned() else {
            if std::mem::take(&mut self.dragging) {
                self.pane_event(editor::Event::CancelPointer);
            }
            return;
        };
        if let Some(rect) = pane_rect(&region, surface) {
            self.pane_event(editor::Event::Frame { rect, surface });
        }
        if self
            .held
            .as_ref()
            .is_none_or(|held| held.id != region.document.id)
        {
            self.load(&region.document);
        }
    }

    fn load(&mut self, document: &Document) {
        if let Some(tab) = self.held.take().and_then(|held| held.tab) {
            if let Ok(revision) = self.pane.editor().document(tab).map(|doc| doc.revision()) {
                self.pane_event(editor::Event::Close { tab, revision });
            }
        }
        self.dragging = false;
        let mut inks: Vec<(Range<usize>, u32)> = document
            .styles
            .iter()
            .map(|(range, style)| (range.clone(), paint_of(*style).1.ink))
            .collect();
        let mut loaded = self.pane_event(editor::Event::Load(document.text.as_bytes()));
        if !matches!(loaded, Outcome::Created(_)) {
            inks.clear();
            let refused = format!(
                "This preview cannot be shown here: its {} bytes are past what the pane holds.",
                document.text.len()
            );
            loaded = self.pane_event(editor::Event::Load(refused.as_bytes()));
        }
        let tab = match loaded {
            Outcome::Created(tab) => {
                self.pane_event(editor::Event::ReadOnly { tab, enabled: true });
                Some(tab)
            }
            _ => None,
        };
        self.held = Some(Held {
            id: document.id,
            tab,
            inks,
        });
        self.dirty = true;
    }

    /// A chord the pane takes while the frame on screen hands it the reading
    /// keys: a scroll, a copy, select-all, or one the panes never bound,
    /// such as Shift and an arrow, which extends its selection. Whether it
    /// took it.
    fn pane_key(&mut self, chord: &str, repeat: bool, clipboard: &mut dyn Clipboard) -> bool {
        if !self.region().is_some_and(|region| region.keys) {
            return false;
        }
        let Some((tab, revision)) = self.showing() else {
            return false;
        };
        if chord == "C-c" || chord == "C-S-c" {
            if !repeat {
                self.copy(tab, revision, clipboard);
            }
            return true;
        }
        let key = view::key(chord);
        if let Some(key) = key {
            let page = self.pane.geometry().grid().1;
            if let Some(rows) = scroll_of(key, page) {
                if !repeat || view::repeats(key) {
                    self.scroll_pane(tab, revision, rows);
                }
                return true;
            }
            if key != Key::Ctrl('a') {
                return false;
            }
        }
        let outcome = self.pane_event(editor::Event::Key {
            tab,
            revision,
            chord,
        });
        if let Outcome::Request { name: "copy", .. } = outcome {
            self.copy(tab, revision, clipboard);
        }
        true
    }

    fn scroll_pane(&mut self, tab: TabId, revision: u64, rows: isize) {
        self.pane_event(editor::Event::Scroll {
            tab,
            revision,
            rows,
            columns: 0,
        });
    }

    /// Offers the pane's selection to the clipboard, at the press being
    /// delivered; what came of it is the status row's note.
    fn copy(&mut self, tab: TabId, revision: u64, clipboard: &mut dyn Clipboard) {
        let note = match Snapshot::capture_selection(self.pane.editor(), tab, revision) {
            Ok(Some(snapshot)) => match clipboard.copy(snapshot.text()) {
                Ok(()) => " copied to the clipboard".to_string(),
                Err(refusal) => format!(" copy refused: {refusal}"),
            },
            Ok(None) => " nothing selected to copy".to_string(),
            Err(PaneError::Limit) => format!(
                " copy refused: the selection is past the clipboard's {} KiB",
                editor_clipboard::MAX_BYTES / 1024
            ),
            Err(error) => format!(" copy refused: {error}"),
        };
        self.note = Some(note);
        self.dirty = true;
    }

    /// A left-button phase for the pane: a press in it, and the drag and
    /// release that follow it wherever they go. Whether it was the pane's.
    fn pane_pointer(&mut self, phase: PointerPhase, x: i64, y: i64, extend: bool) -> bool {
        if !self.dragging {
            let inside = phase == PointerPhase::Press
                && self
                    .surface
                    .zip(self.region())
                    .and_then(|(surface, region)| pane_rect(region, surface))
                    .is_some_and(|rect| rect.contains(x, y));
            if !inside {
                return false;
            }
        }
        let Some((tab, revision)) = self.showing() else {
            self.dragging = false;
            return false;
        };
        self.dragging = phase != PointerPhase::Release;
        self.clicks.cancel();
        // The pane counts a double or triple click by its ticks.
        self.pane_event(editor::Event::Tick(self.clock));
        self.pane_event(editor::Event::Pointer {
            tab,
            revision,
            phase: match phase {
                PointerPhase::Press => editor::PointerPhase::Press,
                PointerPhase::Move => editor::PointerPhase::Move,
                PointerPhase::Release => editor::PointerPhase::Release,
            },
            x,
            cell_x: x,
            y,
            extend,
        });
        true
    }

    /// Hands the worker `event`; a worker that has gone closes the window.
    fn send(&mut self, event: Event) -> Flow {
        if self.events.send(event).is_err() {
            return Flow::Quit;
        }
        self.sent = self.sent.saturating_add(1);
        Flow::Continue
    }

    /// The stamp for an input read at `now`: the newest frame painted whole
    /// at least `DWELL_MS` before it, or none.
    fn stamp(&self, now: Instant) -> u64 {
        self.stamped(now).map_or(0, |shown| shown.generation)
    }

    fn stamped(&self, now: Instant) -> Option<&Shown> {
        let dwell = Duration::from_millis(DWELL_MS);
        self.painted.iter().rev().find(|shown| {
            now.checked_duration_since(shown.at)
                .is_some_and(|d| d >= dwell)
        })
    }

    /// The frame painted now, `generation` with `picks`, was shown whole at
    /// `now`.
    fn shown_at(&mut self, generation: u64, picks: &[(usize, String)], now: Instant) {
        // Of the frames painted longer ago than the dwell, the newest is all
        // the stamp can still name.
        let dwell = Duration::from_millis(DWELL_MS);
        while self.painted.get(1).is_some_and(|shown| {
            now.checked_duration_since(shown.at)
                .is_some_and(|d| d >= dwell)
        }) {
            self.painted.pop_front();
        }
        if self
            .painted
            .back()
            .is_some_and(|newest| newest.generation >= generation)
        {
            return;
        }
        while self.painted.len() >= SHOWN_KEPT {
            self.painted.pop_front();
        }
        self.painted.push_back(Shown {
            generation,
            at: now,
            picks: picks.to_vec(),
        });
    }

    fn press(&mut self, x: i64, y: i64, now: Instant) -> Flow {
        let height = (LINE * self.scale) as i64;
        let Ok(row) = usize::try_from(y.div_euclid(height)) else {
            return Flow::Continue;
        };
        let s = self.scale as i64;
        let nanos =
            u64::try_from(now.saturating_duration_since(self.epoch).as_nanos()).unwrap_or(u64::MAX);
        // A press just after a repaint may have been aimed at the frame
        // before it, so it is taken only where the frame its stamp names and
        // the frame on screen show the same branch on its row: then either
        // was aimed at it. A double click's second press, after the first
        // press's own repaint, is the common case.
        let taken = self.stamped(now).and_then(|then| {
            let on_then = then
                .picks
                .iter()
                .find(|(at, _)| *at == row)
                .map(|(_, refname)| refname.as_str());
            let on_screen = self.shown.as_ref().and_then(|(_, frame)| frame.picked(row));
            on_screen
                .filter(|now| Some(*now) == on_then)
                .map(|refname| (refname.to_string(), then.generation))
        });
        // A press not taken pairs with nothing: a double click is two taken.
        let Some((refname, seen)) = taken else {
            self.clicks.cancel();
            return Flow::Continue;
        };
        let double = self.clicks.completed(row, nanos, x / s, y / s);
        self.send(Event::Pick {
            refname,
            double,
            seen,
        })
    }

    #[cfg(test)]
    fn input_at(&mut self, input: Input<'_>, now: Instant) -> Flow {
        self.input_with(input, now, &mut NoClipboard)
    }

    fn input_with(
        &mut self,
        input: Input<'_>,
        now: Instant,
        clipboard: &mut dyn Clipboard,
    ) -> Flow {
        // A held key's repeats leave the note its first press made.
        let pressed = matches!(
            input,
            Input::Key { repeat: false, .. }
                | Input::Pointer {
                    phase: PointerPhase::Press,
                    ..
                }
        );
        if pressed && self.note.take().is_some() {
            self.dirty = true;
        }
        match input {
            Input::Key { chord, repeat } => {
                if chord == "?" {
                    self.show_keys |= !repeat;
                    return Flow::Continue;
                }
                if self.pane_key(chord, repeat, clipboard) {
                    return Flow::Continue;
                }
                match view::key(chord) {
                    Some(key) if !repeat || view::repeats(key) => {
                        let seen = self.stamp(now);
                        self.send(Event::Key { key, seen })
                    }
                    _ => Flow::Continue,
                }
            }
            Input::Pointer {
                phase,
                x,
                y,
                extend,
                ..
            } => {
                if self.pane_pointer(phase, x, y, extend) || phase != PointerPhase::Press {
                    return Flow::Continue;
                }
                self.press(x, y, now)
            }
            Input::Wheel { rows, .. } if rows != 0 => {
                self.clicks.cancel();
                if let Some((tab, revision)) = self
                    .region()
                    .is_some_and(|region| region.keys)
                    .then(|| self.showing())
                    .flatten()
                {
                    self.scroll_pane(tab, revision, rows);
                    return Flow::Continue;
                }
                let seen = self.stamp(now);
                self.send(Event::Wheel { rows, seen })
            }
            Input::Resize(surface) => {
                self.scale = surface.scale.value();
                self.grid.set(grid(surface));
                self.dirty = true;
                self.clicks.cancel();
                self.send(Event::Redraw)
            }
            Input::Focus(focused) => {
                self.clicks.cancel();
                self.pane_event(editor::Event::Focus(focused));
                Flow::Continue
            }
            Input::CancelPointer => {
                self.clicks.cancel();
                if std::mem::take(&mut self.dragging) {
                    self.pane_event(editor::Event::CancelPointer);
                }
                Flow::Continue
            }
            // The worker closes the window by ending, once whatever it is
            // running has finished: a landing is not cut off halfway. Until
            // then the status row says so.
            Input::Close => {
                if !self.closing {
                    self.closing = true;
                    self.dirty = true;
                }
                self.send(Event::Close)
            }
            Input::Wheel { .. } | Input::Hover(_) | Input::Paste(_) => Flow::Continue,
        }
    }
}

impl Handler for Session {
    fn app_id(&self) -> &str {
        "td-review"
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) -> Flow {
        self.input_with(input, Instant::now(), clipboard)
    }

    fn poll(&mut self, now: u64) -> Flow {
        self.clock = now;
        self.pane_event(editor::Event::Tick(now));
        loop {
            match self.messages.try_recv() {
                Ok(Message::Frame {
                    generation,
                    frame,
                    taken,
                }) => {
                    self.pending = Some((generation, frame));
                    self.taken = taken;
                    self.idle = false;
                    self.dirty = true;
                }
                Ok(Message::Taken { taken }) => {
                    self.taken = taken;
                    self.idle = false;
                }
                Ok(Message::Idle { taken }) => {
                    self.taken = taken;
                    self.idle = true;
                }
                Err(TryRecvError::Empty) => return Flow::Continue,
                Err(TryRecvError::Disconnected) => return Flow::Quit,
            }
        }
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        if self.taken < self.sent {
            AWAITED_MS
        } else if !self.idle {
            WORKING_MS
        } else {
            IDLE_MS
        }
    }

    fn needs_redraw(&self) -> bool {
        self.dirty
    }

    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        if let Some(pending) = self.pending.take() {
            self.shown = Some(pending);
            // A copy was answered on the frame before: painted over this
            // one's last row, it could hide the confirmation it raises.
            self.note = None;
        }
        self.surface = Some(surface);
        self.sync_pane(surface);
        {
            let frame = self.shown.as_ref().map(|(_, frame)| frame);
            // The pane is painted only where it was placed on this surface.
            // A scene the pane cannot make leaves its rows blank rather than
            // closing the window, perhaps mid-landing.
            let scene = match (self.showing(), self.held.as_ref()) {
                (Some(_), Some(held)) if self.pane.geometry().surface() == surface => self
                    .pane
                    .scene(&[])
                    .ok()
                    .map(|scene| scene.inks(&held.inks)),
                _ => None,
            };
            let notice = if self.closing {
                Some(CLOSING)
            } else {
                self.note.as_deref()
            };
            raster
                .paint(
                    &Painting {
                        surface,
                        frame,
                        notice,
                        pane: scene.as_ref().map(|scene| scene as &dyn Composition),
                    },
                    surface.bounds(),
                )
                .map_err(|e| e.to_string())?;
        }
        self.dirty = false;
        if let Some((generation, frame)) = self.shown.take() {
            if fits(&frame, surface) {
                self.shown_at(generation, frame.picks(), Instant::now());
            }
            self.shown = Some((generation, frame));
        }
        Ok(())
    }

    fn notice(&mut self, message: &str) {
        eprintln!("td-review: window: {}", view::scrub(message));
    }

    fn keys(&self) -> Vec<td_ui::keys::Section> {
        keys::sections(self.region().is_some())
    }

    fn take_show_keys(&mut self) -> bool {
        std::mem::take(&mut self.show_keys)
    }
}

/// Runs `app` in a window on the compositor the environment names, until the
/// window closes or the app quits. The worker is joined before this returns:
/// a window that failed still waits for the git it was running.
pub fn run(app: App, title: String) -> io::Result<()> {
    let endpoint = td_ui::wayland::endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
    .map_err(io::Error::other)?;
    let stream = td_ui::wayland::connect(endpoint).map_err(io::Error::other)?;
    let (event_tx, event_rx) = mpsc::channel();
    let (message_tx, message_rx) = mpsc::channel();
    let shared = Arc::new(Grid::new((24, 80)));
    let link = Link::new(event_rx, message_tx, Arc::clone(&shared));
    let worker = thread::Builder::new()
        .name("td-review".into())
        .spawn(move || serve(app, link))?;
    let mut session =
        Session::new(title, event_tx, message_rx, shared).map_err(io::Error::other)?;
    let typeface = td_ui::pinned_face::load_or_note(
        "td-review",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    let shown = td_ui::window::run(&mut session, stream, std::env::temp_dir(), typeface);
    // The worker sees the window go at its next receive, or its next draw.
    drop(session);
    let worked = worker
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("the worker panicked")));
    shown.map_err(io::Error::other)?;
    worked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::Line;
    use td_ui::raster::Scale;

    fn surface(width: usize, height: usize, scale: u8) -> Surface {
        Surface::new(width, height, Scale::new(scale).unwrap()).unwrap()
    }

    #[test]
    fn the_grid_is_the_rows_and_cells_a_surface_holds() {
        // 800 - (4 + 12) = 784 = 98 cells; 600 / 18 = 33 rows.
        assert_eq!(grid(surface(800, 600, 1)), (33, 98));
        assert_eq!(grid(surface(1600, 1200, 2)), (33, 98));
        // Never less than the panes can be laid out on.
        assert_eq!(grid(surface(40, 20, 1)), (MIN_ROWS, MIN_COLS));
    }

    /// A frame laid out for the surface fits it; on one too small to hold its
    /// rows or its width, it does not, whatever the grid's floor laid it out on.
    #[test]
    fn a_frame_fits_only_a_surface_that_holds_it_whole() {
        let laid = |s: Surface| {
            let (rows, cols) = grid(s);
            let mut frame = Frame::new(rows, cols);
            for _ in 0..rows {
                frame.push_text("x", Style::PLAIN);
            }
            frame
        };
        for s in [surface(800, 600, 1), surface(1600, 1200, 2)] {
            assert!(fits(&laid(s), s));
        }
        let tiny = surface(800, 54, 1);
        assert!(!fits(&laid(tiny), tiny), "four rows on a three-row surface");
        let narrow = surface(100, 600, 1);
        assert!(!fits(&laid(narrow), narrow));
        assert!(!fits(&laid(surface(800, 600, 1)), surface(800, 300, 1)));
    }

    /// Every glyph a painting draws: where, which scalar, in what ink.
    fn glyphs(painting: &Painting<'_>) -> Vec<(i64, i64, char, u32)> {
        let mut out = Vec::new();
        painting.emit(painting.surface.bounds(), &mut |draw| {
            if let Primitive::Glyph {
                x,
                y,
                scalar,
                style,
            } = draw.primitive
            {
                out.push((x, y, scalar, style.ink));
            }
        });
        out
    }

    fn fills(painting: &Painting<'_>) -> Vec<(Rect, u32)> {
        let mut out = Vec::new();
        painting.emit(painting.surface.bounds(), &mut |draw| {
            if let Primitive::Fill { rect, color } = draw.primitive {
                out.push((rect, color));
            }
        });
        out
    }

    /// Each row's text starts at the margin on its own line of the grid, in
    /// its slot's ink; a bar fills its row and draws on it in paper.
    #[test]
    fn rows_are_painted_on_their_lines_in_their_styles() {
        let mut frame = Frame::new(4, 40);
        frame.push(&Line::new("ab", Style::bar(CYAN)));
        frame.push(&Line::new("c", Style::fg(RED)));
        frame.push(&Line::new("d", Style::dim()));
        frame.push(&Line::new("e", Style::PLAIN.with_invert()));
        let painting = Painting {
            surface: surface(400, 200, 1),
            frame: Some(&frame),
            notice: None,
            pane: None,
        };
        assert_eq!(
            glyphs(&painting),
            [
                (4, 1, 'a', PAPER),
                (12, 1, 'b', PAPER),
                (4, 19, 'c', MISSPELLED),
                (4, 37, 'd', LINE_NUMBER),
                (4, 55, 'e', INK),
            ]
        );
        let fills = fills(&painting);
        assert!(fills.contains(&(
            Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 18
            },
            SELECTED
        )));
        assert!(fills.contains(&(
            Rect {
                x: 0,
                y: 54,
                width: 400,
                height: 18
            },
            SELECTED_ROW & 0x00ff_ffff
        )));
    }

    /// The closing notice replaces the frame's last row, whatever was there.
    #[test]
    fn the_closing_notice_is_the_last_row() {
        let mut frame = Frame::new(2, 60);
        frame.push(&Line::plain("a"));
        frame.push(&Line::new("z", Style::dim()));
        let painting = Painting {
            surface: surface(600, 100, 1),
            frame: Some(&frame),
            notice: Some(CLOSING),
            pane: None,
        };
        let last: String = glyphs(&painting)
            .into_iter()
            .filter(|&(_, y, ..)| y == 19)
            .map(|(.., c, _)| c)
            .collect();
        assert!(last.starts_with("z closing once"), "{last}");
        assert!(fills(&painting)
            .iter()
            .any(|&(rect, color)| rect.y == 18 && color == ink(YELLOW)));
        // A frame taller than the surface puts it on the surface's last row.
        let mut tall = Frame::new(10, 60);
        for _ in 0..10 {
            tall.push(&Line::plain("a"));
        }
        let painting = Painting {
            surface: surface(600, 40, 1),
            frame: Some(&tall),
            notice: Some(CLOSING),
            pane: None,
        };
        assert!(fills(&painting)
            .iter()
            .any(|&(rect, color)| rect.y == 18 && color == ink(YELLOW)));
    }

    /// Text stops short of the scrollbar's gutter, and the bar is drawn only
    /// over a region with more lines than it shows.
    #[test]
    fn text_is_clipped_short_of_the_scrollbar() {
        let mut frame = Frame::new(3, 200);
        frame.push(&Line::plain("title"));
        frame.scrollbar(2, 10, 4);
        frame.push(&Line::plain("x".repeat(200)));
        let painting = Painting {
            surface: surface(100, 54, 1),
            frame: Some(&frame),
            notice: None,
            pane: None,
        };
        let shown = glyphs(&painting);
        assert!(shown.iter().all(|&(x, ..)| x + 8 <= 100 - 12), "{shown:?}");
        let track = fills(&painting)
            .into_iter()
            .find(|&(rect, color)| color == CHROME && rect.width == 8)
            .map(|(rect, _)| rect);
        assert_eq!(
            track,
            Some(Rect {
                x: 90,
                y: 18,
                width: 8,
                height: 36
            })
        );

        for (visible, total) in [(2, 2), (0, 5)] {
            let mut short = Frame::new(3, 20);
            short.scrollbar(visible, total, 0);
            let painting = Painting {
                surface: surface(100, 54, 1),
                frame: Some(&short),
                notice: None,
                pane: None,
            };
            assert!(!fills(&painting).iter().any(|&(_, color)| color == CHROME));
        }
    }

    fn link() -> (Link, Sender<Event>, Receiver<Message>) {
        let (event_tx, event_rx) = mpsc::channel();
        let (message_tx, message_rx) = mpsc::channel();
        let link = Link::new(event_rx, message_tx, Arc::new(Grid::new((24, 80))));
        (link, event_tx, message_rx)
    }

    fn key(c: char, seen: u64) -> Event {
        Event::Key {
            key: Key::Char(c),
            seen,
        }
    }

    /// The drain is by frame: an input stamped before the frame drawn last is
    /// dropped, one stamped with it is taken, and every input dropped is
    /// still counted as taken, so the window stops waiting for it.
    #[test]
    fn a_drain_drops_the_inputs_made_before_the_frame_it_follows() {
        let (mut link, events, messages) = link();
        link.draw(Frame::new(1, 1)).unwrap();
        link.draw(Frame::new(1, 1)).unwrap();
        link.drain_input().unwrap();
        for event in [
            key('y', 1),
            Event::Pick {
                refname: "origin/x".into(),
                double: false,
                seen: 0,
            },
            Event::Wheel { rows: 1, seen: 1 },
            Event::Redraw,
            key('n', 2),
            Event::Close,
        ] {
            events.send(event).unwrap();
        }
        assert_eq!(link.next(), Some(Event::Redraw));
        assert_eq!(link.next(), Some(key('n', 2)));
        assert_eq!(link.next(), Some(Event::Close));
        assert_eq!(link.taken, 6);
        drop(events);
        assert_eq!(link.next(), None);
        let generations: Vec<u64> = messages
            .try_iter()
            .filter_map(|m| match m {
                Message::Frame { generation, .. } => Some(generation),
                _ => None,
            })
            .collect();
        assert_eq!(generations, [1, 2]);
    }

    /// Without a drain nothing is dropped, however old its frame.
    #[test]
    fn inputs_are_taken_in_order_until_a_drain() {
        let (mut link, events, _messages) = link();
        link.draw(Frame::new(1, 1)).unwrap();
        events.send(key('j', 0)).unwrap();
        events.send(key('k', 1)).unwrap();
        assert_eq!(link.next(), Some(key('j', 0)));
        assert_eq!(link.next(), Some(key('k', 1)));
    }

    /// The worker says it took an input when it takes one, and that it is idle
    /// before it blocks, with every input counted.
    #[test]
    fn the_worker_says_what_it_took_and_when_it_waits() {
        let (mut link, events, messages) = link();
        events.send(Event::Redraw).unwrap();
        let worker = thread::spawn(move || (link.next(), link.next()));
        assert!(matches!(messages.recv(), Ok(Message::Taken { taken: 1 })));
        // The second `next` finds nothing queued: it says so, then waits.
        assert!(matches!(messages.recv(), Ok(Message::Idle { taken: 1 })));
        events.send(Event::Close).unwrap();
        assert_eq!(
            worker.join().unwrap(),
            (Some(Event::Redraw), Some(Event::Close))
        );
    }

    fn session() -> (Session, Receiver<Event>, Sender<Message>) {
        let (event_tx, event_rx) = mpsc::channel();
        let (message_tx, message_rx) = mpsc::channel();
        let session = Session::new(
            "td-review".into(),
            event_tx,
            message_rx,
            Arc::new(Grid::new((24, 80))),
        )
        .unwrap();
        (session, event_rx, message_tx)
    }

    fn press(chord: &str, repeat: bool) -> Input<'_> {
        Input::Key { chord, repeat }
    }

    fn ms(at: Instant, ms: u64) -> Instant {
        at + Duration::from_millis(ms)
    }

    /// An input is stamped with the newest frame painted whole a dwell before
    /// it was read: one read just after a paint may have been made before it.
    #[test]
    fn inputs_are_stamped_with_a_frame_shown_a_dwell_before() {
        let (mut session, events, _messages) = session();
        let t = session.epoch;
        session.input_at(press("D", false), t);
        session.shown_at(1, &[], ms(t, 10));
        session.input_at(press("j", false), ms(t, 50));
        session.input_at(press("y", false), ms(t, 10 + DWELL_MS));
        session.shown_at(2, &[], ms(t, 200));
        session.shown_at(3, &[], ms(t, 210));
        session.input_at(press("n", false), ms(t, 250));
        session.input_at(press("q", false), ms(t, 300));
        // A generation painted again is not painted anew.
        session.shown_at(3, &[], ms(t, 400));
        session.input_at(press("k", false), ms(t, 450));
        let stamps: Vec<(Key, u64)> = events
            .try_iter()
            .filter_map(|e| match e {
                Event::Key { key, seen } => Some((key, seen)),
                _ => None,
            })
            .collect();
        assert_eq!(
            stamps,
            [
                (Key::Char('D'), 0),
                (Key::Char('j'), 0),
                (Key::Char('y'), 1),
                (Key::Char('n'), 1),
                (Key::Char('q'), 3),
                (Key::Char('k'), 3),
            ]
        );
        assert!(session.painted.len() <= 2, "{:?}", session.painted);
    }

    /// A frame painted on a surface it does not fit is not counted as shown.
    #[test]
    fn a_clipped_frame_is_not_shown() {
        let (mut session, _events, messages) = session();
        let mut frame = Frame::new(4, 20);
        for _ in 0..4 {
            frame.push_text("row", Style::PLAIN);
        }
        messages
            .send(Message::Frame {
                generation: 1,
                frame,
                taken: 0,
            })
            .unwrap();
        session.poll(0);
        let (font, mut pixels) = (td_ui::font::pinned().unwrap(), vec![0; 800 * 54 * 4]);
        let small = surface(800, 54, 1);
        let mut raster = Raster::new(&mut pixels, &font, small, 800 * 4).unwrap();
        session.paint(&mut raster, small).unwrap();
        assert!(session.painted.is_empty());
        let mut pixels = vec![0; 800 * 600 * 4];
        let big = surface(800, 600, 1);
        let mut raster = Raster::new(&mut pixels, &font, big, 800 * 4).unwrap();
        session.paint(&mut raster, big).unwrap();
        assert_eq!(session.painted.back().map(|s| s.generation), Some(1));
    }

    /// A held key repeats only where it moves something, and a chord the
    /// panes do not read is not sent at all.
    #[test]
    fn only_movement_is_sent_on_the_repeat_clock() {
        let (mut session, events, _messages) = session();
        let board = &mut td_ui::window::NoClipboard;
        for (chord, repeat) in [
            ("j", true),
            ("D", true),
            ("y", true),
            ("M-x", false),
            ("F1", false),
            ("D", false),
        ] {
            session.input(press(chord, repeat), board);
        }
        let keys: Vec<Key> = events
            .try_iter()
            .filter_map(|e| match e {
                Event::Key { key, .. } => Some(key),
                _ => None,
            })
            .collect();
        assert_eq!(keys, [Key::Char('j'), Key::Char('D')]);
    }

    /// `?` asks for the window's key list, once a press, on the list's frame
    /// and the review's alike; the worker never hears it. `F1` is the
    /// window's own before it reaches the handler, which sends nothing for it
    /// either. Under a prompt and the filter: the test after this one.
    #[test]
    fn a_question_mark_opens_the_key_list_and_reaches_no_worker() {
        let (mut session, events, _messages) = session();
        let board = &mut td_ui::window::NoClipboard;
        assert!(!session.take_show_keys());
        session.input(press("?", false), board);
        assert!(session.take_show_keys());
        assert!(!session.take_show_keys(), "an edge, taken once");
        session.input(press("?", true), board);
        assert!(!session.take_show_keys(), "a repeat asks for nothing");
        let document = Arc::new(Document::new(1, &[Line::plain("hello")]));
        showing(&mut session, 1, reviewing(&document, true));
        session.input(press("?", false), board);
        assert!(session.take_show_keys());
        assert!(events.try_recv().is_err(), "the worker saw no `?`");
    }

    /// Polls `session` until the worker's frame shows `text`, puts each frame
    /// on screen as it comes, and returns when inputs read after it are
    /// stamped with it: a dwell after it was shown.
    fn until(session: &mut Session, text: &str) -> Instant {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            session.poll(0);
            if let Some((generation, frame)) = session.pending.take() {
                let found = frame.text().contains(text);
                let at = Instant::now();
                session.shown_at(generation, frame.picks(), at);
                showing(session, generation, frame);
                if found {
                    return ms(at, DWELL_MS);
                }
            }
            assert!(Instant::now() < deadline, "no frame with {text:?}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// With the worker's own frames on screen, `?` under a squash
    /// confirmation and while the branch filter is typed opens the key list
    /// and sends the worker nothing: the confirmation is still up for the
    /// `n` that answers it, and the filter holds only what was typed.
    #[test]
    fn a_question_mark_under_a_prompt_or_the_filter_reaches_no_worker() {
        let (event_tx, event_rx) = mpsc::channel();
        let (message_tx, message_rx) = mpsc::channel();
        let grid = Arc::new(Grid::new((24, 80)));
        let link = Link::new(event_rx, message_tx, Arc::clone(&grid));
        let worker = thread::spawn(move || serve(crate::app::reviewing_fixture(), link));
        let mut session = Session::new("td-review".into(), event_tx, message_rx, grid).unwrap();
        let question = |session: &mut Session, at: Instant| {
            let sent = session.sent;
            session.input_at(press("?", false), at);
            assert!(session.take_show_keys());
            assert_eq!(session.sent, sent, "the worker was sent the `?`");
        };

        let at = until(&mut session, " review ");
        session.input_at(press("s", false), at);
        let at = until(&mut session, "squash into one commit");
        question(&mut session, at);
        session.input_at(press("n", false), at);
        let at = until(&mut session, "cancelled");

        session.input_at(press("q", false), at);
        let at = until(&mut session, " enter review ");
        session.input_at(press("/", false), at);
        let at = until(&mut session, " filter: _");
        question(&mut session, at);
        session.input_at(press("x", false), at);
        until(&mut session, " filter: x_");

        session.input_at(Input::Close, Instant::now());
        assert!(worker.join().unwrap().is_ok());
    }

    /// The list's sections are the review's first while a review is on
    /// screen, and the branch list's otherwise.
    #[test]
    fn the_key_list_leads_with_the_pane_on_screen() {
        let (mut session, _events, _messages) = session();
        let first = |session: &Session| session.keys().first().map(|s| s.title);
        assert_eq!(first(&session), Some("branch list"));
        let document = Arc::new(Document::new(1, &[Line::plain("hello")]));
        showing(&mut session, 1, reviewing(&document, true));
        assert_eq!(first(&session), Some("review"));
        assert_eq!(session.keys().len(), 5);
        showing(&mut session, 2, listing(&["origin/a"]));
        assert_eq!(first(&session), Some("branch list"));
    }

    fn listing(names: &[&str]) -> Frame {
        let mut frame = Frame::new(8, 40);
        frame.push_text("title", Style::PLAIN);
        frame.push_text("header", Style::PLAIN);
        for name in names {
            frame.pick(name);
            frame.push_text(name, Style::PLAIN);
        }
        frame
    }

    /// A press names the branch the frame on screen shows on its row, at the
    /// surface's scale — not a frame received and not yet painted — and a
    /// second press there soon after is a double click. A press on a row
    /// showing no branch is nothing. Within the dwell after a repaint, a press
    /// is taken only where the frame before showed the same branch there: a
    /// double click across the first press's own repaint is, a row something
    /// else slid onto is not.
    #[test]
    fn a_press_picks_the_branch_on_screen_under_it() {
        let (mut session, events, _messages) = session();
        let t = session.epoch;
        session.input_at(Input::Resize(surface(800, 600, 2)), t);
        let on_screen = listing(&["origin/a", "origin/b"]);
        session.shown_at(1, on_screen.picks(), t);
        session.shown = Some((1, on_screen));
        session.pending = Some((2, listing(&["origin/z"])));
        let click = |session: &mut Session, y: i64, at: u64| {
            session.input_at(
                Input::Pointer {
                    phase: PointerPhase::Press,
                    x: 10,
                    y,
                    extend: false,
                    follow: false,
                },
                ms(t, at),
            )
        };
        click(&mut session, 36 * 2 + 5, 1000);
        // The first press's frame comes back, the same rows, just painted.
        let again = listing(&["origin/a", "origin/b"]);
        session.shown_at(2, again.picks(), ms(t, 1010));
        session.shown = Some((2, again));
        click(&mut session, 36 * 2 + 6, 1050);
        click(&mut session, 36 * 3 + 6, 3000);
        click(&mut session, 36, 4000);
        click(&mut session, -1, 5000);
        // A row another branch slid onto, within the dwell: nothing.
        let slid = listing(&["origin/b", "origin/c"]);
        session.shown_at(3, slid.picks(), ms(t, 6000));
        session.shown = Some((3, slid));
        click(&mut session, 36 * 2 + 5, 6000 + DWELL_MS / 2);
        // After the dwell it is the branch on screen.
        click(&mut session, 36 * 2 + 5, 6000 + DWELL_MS * 2);
        let picks: Vec<(String, bool)> = events
            .try_iter()
            .filter_map(|e| match e {
                Event::Pick {
                    refname, double, ..
                } => Some((refname, double)),
                _ => None,
            })
            .collect();
        assert_eq!(
            picks,
            [
                ("origin/a".to_string(), false),
                ("origin/a".to_string(), true),
                ("origin/b".to_string(), false),
                ("origin/b".to_string(), false),
            ]
        );
        assert_eq!(session.grid.get(), (16, 48));
    }

    /// The pace: brief while an input is on its way, longer while the worker
    /// works, idle once it waits; closing says so on the status row until the
    /// worker ends, and a worker that has gone closes the window.
    #[test]
    fn the_window_waits_by_what_the_worker_is_doing() {
        let (mut session, events, messages) = session();
        let board = &mut td_ui::window::NoClipboard;
        session.input(press("j", false), board);
        assert_eq!(session.wait_ms(0), AWAITED_MS);
        messages.send(Message::Taken { taken: 1 }).unwrap();
        session.poll(0);
        assert_eq!(session.wait_ms(0), WORKING_MS);
        messages.send(Message::Idle { taken: 1 }).unwrap();
        session.poll(0);
        assert_eq!(session.wait_ms(0), IDLE_MS);
        session.dirty = false;
        assert_eq!(session.input(Input::Close, board), Flow::Continue);
        assert!(session.closing && session.needs_redraw());
        assert_eq!(events.try_iter().last(), Some(Event::Close));
        drop(messages);
        assert_eq!(session.poll(0), Flow::Quit);
    }

    /// The whole path a confirmation is answered on: a `y` made before the
    /// squash prompt was shown is dropped, the `n` made after it answers it,
    /// and the landing a taken `y` would have run (against no repository,
    /// so an error) never starts.
    #[test]
    fn a_confirmation_is_answered_only_after_it_was_shown() {
        let (event_tx, event_rx) = mpsc::channel();
        let (message_tx, message_rx) = mpsc::channel();
        let link = Link::new(event_rx, message_tx, Arc::new(Grid::new((24, 80))));
        let app = crate::app::reviewing_fixture();
        let worker = thread::spawn(move || serve(app, link));
        let frame_with = |text: &str| loop {
            match message_rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Message::Frame {
                    generation, frame, ..
                }) => {
                    if frame.text().contains(text) {
                        return generation;
                    }
                }
                Ok(_) => {}
                Err(e) => panic!("no frame with {text:?}: {e}"),
            }
        };
        let before = frame_with("review");
        event_tx.send(key('s', before)).unwrap();
        event_tx.send(key('y', before)).unwrap();
        let prompt = frame_with("squash into one commit");
        event_tx.send(key('n', prompt)).unwrap();
        frame_with("cancelled");
        event_tx.send(Event::Close).unwrap();
        assert!(worker.join().unwrap().is_ok(), "the landing never ran");
    }

    /// A clipboard that keeps what it was offered.
    #[derive(Default)]
    struct Kept(Option<Arc<str>>);

    impl Clipboard for Kept {
        fn available(&self) -> bool {
            true
        }
        fn has_text(&self) -> bool {
            false
        }
        fn pasting(&self) -> bool {
            false
        }
        fn copy(&mut self, text: Arc<str>) -> Result<(), td_ui::window::Refusal> {
            self.0 = Some(text);
            Ok(())
        }
        fn paste(&mut self) -> Result<(), td_ui::window::Refusal> {
            Err(td_ui::window::Refusal::NoSelection)
        }
    }

    /// A review frame: a title, `document` in the pane below it, a footer.
    fn reviewing(document: &Arc<Document>, keys: bool) -> Frame {
        let mut frame = Frame::new(20, 80);
        frame.push(&Line::new(" review", Style::bar(CYAN)));
        frame.pane(18, Arc::clone(document), keys);
        frame.push(&Line::new(" keys", Style::dim().with_invert()));
        frame
    }

    /// `frame` on screen on an 800 by 600 surface, the pane placed for it.
    fn showing(session: &mut Session, generation: u64, frame: Frame) {
        let surface = surface(800, 600, 1);
        session.shown = Some((generation, frame));
        session.surface = Some(surface);
        session.sync_pane(surface);
    }

    fn drag(session: &mut Session, from: (i64, i64), to: (i64, i64)) {
        for (phase, (x, y)) in [
            (PointerPhase::Press, from),
            (PointerPhase::Move, to),
            (PointerPhase::Release, to),
        ] {
            session.input_at(
                Input::Pointer {
                    phase,
                    x,
                    y,
                    extend: false,
                    follow: false,
                },
                Instant::now(),
            );
        }
    }

    /// The pane's first document row's cell `column`, in surface pixels.
    fn cell(session: &Session, column: i64) -> (i64, i64) {
        let doc = session.pane.geometry().document();
        (doc.x + column * CELL_WIDTH as i64, doc.y + 8)
    }

    fn selected(session: &Session) -> std::ops::Range<usize> {
        let (tab, _) = session.showing().unwrap();
        session
            .pane
            .editor()
            .document(tab)
            .unwrap()
            .selection()
            .range()
    }

    /// A drag in the review's pane selects, and `C-c` offers the selection to
    /// the clipboard at that key press, saying so on the status row; the
    /// worker sees neither. Under a prompt `C-c` is the prompt's again.
    #[test]
    fn a_drag_in_the_pane_selects_and_c_c_copies_it() {
        let (mut session, events, _messages) = session();
        let document = Arc::new(Document::new(1, &[Line::plain("hello world")]));
        showing(&mut session, 1, reviewing(&document, true));
        let (from, to) = (cell(&session, 0), cell(&session, 5));
        drag(&mut session, from, to);
        assert_eq!(selected(&session), 0..5);
        let mut kept = Kept::default();
        session.input_with(press("C-c", false), Instant::now(), &mut kept);
        assert_eq!(kept.0.as_deref(), Some("hello"));
        assert_eq!(session.note.as_deref(), Some(" copied to the clipboard"));
        let mut held = Kept::default();
        session.input_with(press("C-c", true), Instant::now(), &mut held);
        assert!(held.0.is_none(), "a repeat copies nothing");
        assert!(session.note.is_some(), "and leaves the note");
        session.input_with(press("C-S-c", false), Instant::now(), &mut kept);
        assert!(events.try_recv().is_err(), "the worker saw nothing");

        showing(&mut session, 2, reviewing(&document, false));
        let mut untouched = Kept::default();
        session.input_with(press("C-c", false), Instant::now(), &mut untouched);
        assert!(untouched.0.is_none());
        assert_eq!(
            events.try_iter().next(),
            Some(Event::Key {
                key: Key::Ctrl('c'),
                seen: 0
            })
        );
        assert!(session.note.is_none(), "a key clears the note");
        session.input_at(press("j", false), Instant::now());
        assert!(
            matches!(
                events.try_recv(),
                Ok(Event::Key {
                    key: Key::Char('j'),
                    ..
                })
            ),
            "under a prompt j is the worker's"
        );
    }

    /// A preview past what the pane holds is a line saying so, uninked.
    #[test]
    fn a_preview_past_the_pane_says_so() {
        let (mut session, _events, _messages) = session();
        let huge = Line::new("+".repeat(16 * 1024 * 1024 + 1), Style::fg(GREEN));
        let document = Arc::new(Document::new(1, &[huge]));
        showing(&mut session, 1, reviewing(&document, true));
        let (tab, _) = session.showing().unwrap();
        let shown = session
            .pane
            .editor()
            .document(tab)
            .unwrap()
            .text()
            .to_string();
        assert!(
            shown.starts_with("This preview cannot be shown here"),
            "{shown}"
        );
        assert!(session.held.as_ref().unwrap().inks.is_empty());
    }

    /// With nothing selected a copy offers nothing and says so.
    #[test]
    fn a_copy_with_nothing_selected_says_so() {
        let (mut session, _events, _messages) = session();
        let document = Arc::new(Document::new(1, &[Line::plain("hello")]));
        showing(&mut session, 1, reviewing(&document, true));
        let mut kept = Kept::default();
        session.input_with(press("C-c", false), Instant::now(), &mut kept);
        assert!(kept.0.is_none());
        assert_eq!(session.note.as_deref(), Some(" nothing selected to copy"));
    }

    /// The reading keys and the wheel scroll the pane on the window thread
    /// while the frame hands it them; a key that decides something, and every
    /// key on a frame without a pane, is the worker's.
    #[test]
    fn the_reading_keys_scroll_the_pane() {
        let (mut session, events, _messages) = session();
        let lines: Vec<Line> = (0..200).map(|i| Line::plain(format!("{i}"))).collect();
        let document = Arc::new(Document::new(1, &lines));
        showing(&mut session, 1, reviewing(&document, true));
        let tab = session.showing().unwrap().0;
        let row = |session: &Session| session.pane.tab_view(tab).unwrap().viewport.origin().row;
        session.input_at(press("j", false), Instant::now());
        assert_eq!(row(&session), 1);
        session.input_at(press("G", false), Instant::now());
        let end = row(&session);
        assert!(end > 100, "{end}");
        session.input_at(press("g", false), Instant::now());
        assert_eq!(row(&session), 0);
        session.input_at(
            Input::Wheel {
                rows: 3,
                columns: 0,
            },
            Instant::now(),
        );
        assert_eq!(row(&session), 3);
        assert!(events.try_recv().is_err(), "the worker saw no scroll");
        session.input_at(press("s", false), Instant::now());
        assert!(matches!(
            events.try_recv(),
            Ok(Event::Key {
                key: Key::Char('s'),
                ..
            })
        ));

        showing(&mut session, 2, listing(&["origin/a"]));
        session.input_at(press("j", false), Instant::now());
        assert!(matches!(
            events.try_recv(),
            Ok(Event::Key {
                key: Key::Char('j'),
                ..
            })
        ));
    }

    /// The same document across frames keeps the pane's place and selection;
    /// a new one is loaded afresh, the old one closed.
    #[test]
    fn a_new_document_is_loaded_afresh_and_the_same_one_kept() {
        let (mut session, _events, _messages) = session();
        let first = Arc::new(Document::new(1, &[Line::plain("hello world")]));
        showing(&mut session, 1, reviewing(&first, true));
        let (from, to) = (cell(&session, 0), cell(&session, 5));
        drag(&mut session, from, to);
        showing(&mut session, 2, reviewing(&first, false));
        assert_eq!(selected(&session), 0..5);
        let second = Arc::new(Document::new(2, &[Line::plain("hello world")]));
        showing(&mut session, 3, reviewing(&second, true));
        assert!(selected(&session).is_empty());
        assert_eq!(session.pane.editor().tabs().count(), 1);
    }

    /// The pane paints the document over the frame's rows, each styled line
    /// in its ink.
    #[test]
    fn the_pane_paints_diff_lines_in_their_ink() {
        let (mut session, _events, _messages) = session();
        let lines = [Line::plain("x"), Line::new("+y", Style::fg(GREEN))];
        let document = Arc::new(Document::new(1, &lines));
        showing(&mut session, 1, reviewing(&document, true));
        let held = session.held.as_ref().unwrap();
        let scene = session.pane.scene(&[]).unwrap().inks(&held.inks);
        let (_, frame) = session.shown.as_ref().unwrap();
        let painting = Painting {
            surface: surface(800, 600, 1),
            frame: Some(frame),
            notice: None,
            pane: Some(&scene),
        };
        let inks: Vec<(char, u32)> = glyphs(&painting)
            .into_iter()
            .map(|(_, _, scalar, ink)| (scalar, ink))
            .collect();
        assert!(inks.contains(&('x', INK)), "{inks:?}");
        assert!(inks.contains(&('y', ink(GREEN))), "{inks:?}");
    }

    /// Through the window's own turn: a frame the worker sent is taken by a
    /// poll and painted, the pane placed and painted with it, and the frame
    /// stays on screen for the drag that follows and the paints after it.
    #[test]
    fn a_painted_review_frame_takes_a_drag_into_its_pane() {
        let (mut session, _events, messages) = session();
        let document = Arc::new(Document::new(1, &[Line::plain("hello world")]));
        messages
            .send(Message::Frame {
                generation: 1,
                frame: reviewing(&document, true),
                taken: 0,
            })
            .unwrap();
        assert_eq!(session.poll(0), Flow::Continue);
        let surface = surface(800, 600, 1);
        let font = td_ui::font::pinned().unwrap();
        let mut pixels = vec![0; 800 * 600 * 4];
        let mut raster = Raster::new(&mut pixels, &font, surface, 800 * 4).unwrap();
        session.paint(&mut raster, surface).unwrap();
        let (from, to) = (cell(&session, 0), cell(&session, 5));
        drag(&mut session, from, to);
        assert_eq!(selected(&session), 0..5);
        session.paint(&mut raster, surface).unwrap();
        assert_eq!(selected(&session), 0..5);
        assert!(session.showing().is_some());
    }

    /// A copy's note is the frame's it was made on. `s` sent, then `C-c`
    /// before the squash prompt's frame is painted: the note goes when that
    /// frame does, so the prompt bar it raised is what is on screen when the
    /// frame counts as shown.
    #[test]
    fn a_new_frame_clears_a_copy_note_before_it_can_cover_a_prompt() {
        let (mut session, events, messages) = session();
        let document = Arc::new(Document::new(1, &[Line::plain("hello")]));
        let surface = surface(800, 600, 1);
        let font = td_ui::font::pinned().unwrap();
        let mut pixels = vec![0; 800 * 600 * 4];
        let mut raster = Raster::new(&mut pixels, &font, surface, 800 * 4).unwrap();
        let frame = |generation, keys| Message::Frame {
            generation,
            frame: reviewing(&document, keys),
            taken: 0,
        };
        messages.send(frame(1, true)).unwrap();
        session.poll(0);
        session.paint(&mut raster, surface).unwrap();
        session.input_at(press("s", false), Instant::now());
        assert!(events.try_recv().is_ok(), "s is the worker's");
        // The prompt's frame arrives, and C-c is read before it is painted.
        messages.send(frame(2, false)).unwrap();
        session.poll(0);
        session.input_at(press("C-c", false), Instant::now());
        assert!(session.note.is_some());
        session.paint(&mut raster, surface).unwrap();
        assert!(session.note.is_none());
        assert!(!session.region().unwrap().keys);
    }
}
