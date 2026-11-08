//! The browser in its window (DESIGN.md §8): the address row, the page
//! and the status row; the history; and each navigation's fetch and
//! decoding on a thread of its own, its document taken when the window
//! next polls. Rendering at the page's width is the window's.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;

use td_ui::chrome::{Status, TextEntry, ROW};
use td_ui::entry_model::{Action, EntryModel};
use td_ui::keys::Section;
use td_ui::picture::Fitted;
use td_ui::raster::{fill, Raster, Rect, Surface, CHROME, PAPER};
use td_ui::window::{Clipboard, Flow, Handler, Input, PointerPhase};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};
use td_url::{Reference, Url};

use crate::document::{self, Document, Rendered};
use crate::fetch::{self, Fetcher};
use crate::view::{self, View};

/// The longest address the row holds, in characters.
const MAX_ADDRESS: usize = 4096;
/// The longest find query.
const MAX_QUERY: usize = 256;
/// The most pages kept behind or ahead.
pub const MAX_HISTORY: usize = 100;
/// The most fetches running at once, stopped ones included: a stopped
/// fetch runs on until the service answers or times out.
pub const MAX_WORKERS: usize = 4;
/// How long a turn waits while a fetch is out, and otherwise.
const LOADING_WAIT_MS: u64 = 50;
const IDLE_WAIT_MS: u64 = 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Address,
    Page,
    Find,
}

/// A place in the history: the URL, the fragment it named and how far
/// down it was read.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    url: Url,
    fragment: Option<String>,
    top: usize,
}

/// What a navigation does to the history once its page arrives, and only
/// then: one stopped, refused or overtaken leaves the history as it was.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Motion {
    New,
    Back,
    Forward,
    Reload,
}

/// A navigation's answer: the document and the fragment it names.
type Arrival = (Document, Option<String>);

/// A fetch out on its thread.
struct Loading {
    url: Url,
    fragment: Option<String>,
    motion: Motion,
    /// The line to show first once it is in, for a history step.
    top: usize,
    answer: Receiver<Arrival>,
}

/// The page shown, and its rendering at the width it was made for.
struct Current {
    doc: Document,
    fragment: Option<String>,
    view: View,
    rendered: Rendered,
    rendered_for: Option<usize>,
    fitted: Option<Fitted>,
}

pub struct App {
    fetcher: Arc<dyn Fetcher>,
    workers: Arc<AtomicUsize>,
    surface: Surface,
    focus: Focus,
    address: EntryModel,
    query: EntryModel,
    current: Current,
    back: Vec<Entry>,
    forward: Vec<Entry>,
    loading: Option<Loading>,
    /// What the last input made happen, said until the next input.
    message: Option<String>,
    pointer: Option<(i64, i64)>,
    hover: Option<usize>,
    title: String,
    redraw: bool,
    quit: bool,
    show_keys: bool,
}

/// Counts a worker until its thread ends, however it ends.
struct Worker(Arc<AtomicUsize>);

impl Drop for Worker {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Fetches `url` and makes its document, on a worker's thread.
fn arrive(fetcher: &dyn Fetcher, url: Url, fragment: Option<String>) -> Arrival {
    match fetch::navigate(fetcher, url.clone()) {
        Ok(loaded) => {
            let content_type = loaded.reply.header("content-type").map(str::to_string);
            let doc = Document::from_reply(
                loaded.url,
                loaded.reply.status,
                content_type.as_deref(),
                loaded.reply.body,
            );
            (doc, loaded.fragment.or(fragment))
        }
        Err(why) => (Document::error(Some(url), &why), fragment),
    }
}

impl App {
    pub fn new(fetcher: Arc<dyn Fetcher>, surface: Surface) -> Result<Self, String> {
        let entry = |limit| EntryModel::new(limit).map_err(|why| format!("{why:?}"));
        Ok(Self {
            fetcher,
            workers: Arc::new(AtomicUsize::new(0)),
            surface,
            focus: Focus::Address,
            address: entry(MAX_ADDRESS)?,
            query: entry(MAX_QUERY)?,
            current: Current {
                doc: Document::blank(),
                fragment: None,
                view: View::default(),
                rendered: Rendered::default(),
                rendered_for: None,
                fitted: None,
            },
            back: Vec::new(),
            forward: Vec::new(),
            loading: None,
            message: None,
            pointer: None,
            hover: None,
            title: "td-web".into(),
            redraw: true,
            quit: false,
            show_keys: false,
        })
    }

    /// Starts at `address`, as if it were typed.
    pub fn open(&mut self, address: &str) {
        self.go(address);
    }

    // Geometry.

    fn scale(&self) -> i64 {
        self.surface.scale.value() as i64
    }

    fn address_rect(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.surface.width as u32,
            height: (ROW as i64 * self.scale()) as u32,
        }
    }

    fn status(&self) -> Status {
        Status::new(self.surface)
    }

    fn page_rect(&self) -> Rect {
        let top = i64::from(self.address_rect().height);
        let bottom = self.status().rect().y.max(top);
        Rect {
            x: 0,
            y: top,
            width: self.surface.width as u32,
            height: (bottom - top) as u32,
        }
    }

    fn cell(&self) -> (i64, i64) {
        (
            CELL_WIDTH as i64 * self.scale(),
            CELL_HEIGHT as i64 * self.scale(),
        )
    }

    /// The page's text columns, a cell's inset each side.
    fn columns(&self) -> usize {
        let (cw, _) = self.cell();
        ((i64::from(self.page_rect().width) / cw).max(3) - 2) as usize
    }

    /// The page's rows shown whole.
    fn rows(&self) -> usize {
        let (_, ch) = self.cell();
        (i64::from(self.page_rect().height) / ch).max(1) as usize
    }

    // Navigation.

    /// Navigates to what the person typed.
    fn go(&mut self, typed: &str) {
        match td_url::address(typed) {
            Ok(reference) => self.follow(reference),
            Err(why) => self.message = Some(why),
        }
    }

    fn follow(&mut self, reference: Reference) {
        match reference {
            Reference::Fetch { url, fragment } => {
                let same = self.current.doc.url.as_ref() == Some(&url);
                if same && fragment.is_some() && self.loading.is_none() {
                    // td-html keeps no anchors, so a fragment of this
                    // page is only said, not scrolled to.
                    self.current.fragment = fragment;
                    self.show_address();
                    self.focus = Focus::Page;
                } else {
                    self.load(url, fragment, Motion::New, 0);
                }
            }
            Reference::Other { scheme } => {
                self.message = Some(format!(
                    "td-web does not open {} addresses",
                    document::visible(&scheme)
                ));
            }
        }
    }

    fn load(&mut self, url: Url, fragment: Option<String>, motion: Motion, top: usize) {
        if self.workers.load(Ordering::SeqCst) >= MAX_WORKERS {
            self.message = Some("earlier pages are still loading; try again shortly".into());
            return;
        }
        let (send, answer) = mpsc::channel();
        let fetcher = Arc::clone(&self.fetcher);
        let (target, named) = (url.clone(), fragment.clone());
        self.workers.fetch_add(1, Ordering::SeqCst);
        let worker = Worker(Arc::clone(&self.workers));
        let spawned = std::thread::Builder::new()
            .name("td-web fetch".into())
            .spawn(move || {
                let _worker = worker;
                let _ = send.send(arrive(fetcher.as_ref(), target, named));
            });
        if let Err(why) = spawned {
            self.message = Some(format!("the page could not be fetched: {why}"));
            return;
        }
        self.loading = Some(Loading {
            url,
            fragment,
            motion,
            top,
            answer,
        });
        self.focus = Focus::Page;
        self.show_address();
    }

    /// Takes a finished fetch's document, and moves the history as its
    /// navigation asked.
    fn settle(&mut self) {
        let Some(loading) = &self.loading else {
            return;
        };
        let (doc, fragment) = match loading.answer.try_recv() {
            Ok(arrival) => arrival,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => (
                Document::error(
                    Some(loading.url.clone()),
                    "the fetch ended without an answer",
                ),
                loading.fragment.clone(),
            ),
        };
        let Some(loading) = self.loading.take() else {
            return;
        };
        let here = self.entry();
        match loading.motion {
            Motion::New => {
                if let Some(here) = here {
                    push_bounded(&mut self.back, here);
                }
                self.forward.clear();
            }
            Motion::Back => {
                self.back.pop();
                if let Some(here) = here {
                    push_bounded(&mut self.forward, here);
                }
            }
            Motion::Forward => {
                self.forward.pop();
                if let Some(here) = here {
                    push_bounded(&mut self.back, here);
                }
            }
            Motion::Reload => {}
        }
        self.install(doc, fragment, loading.top);
    }

    fn install(&mut self, doc: Document, fragment: Option<String>, top: usize) {
        self.title = match (&doc.title, &doc.url) {
            (Some(title), _) => format!("{title} - td-web"),
            (None, Some(url)) => format!("{} - td-web", url.text()),
            (None, None) => "td-web".into(),
        };
        self.current = Current {
            doc,
            fragment,
            view: View {
                top,
                ..View::default()
            },
            rendered: Rendered::default(),
            rendered_for: None,
            fitted: None,
        };
        self.relayout();
        self.show_address();
    }

    /// The current page as a history entry; none for the start page.
    fn entry(&self) -> Option<Entry> {
        Some(Entry {
            url: self.current.doc.url.clone()?,
            fragment: self.current.fragment.clone(),
            top: self.current.view.top,
        })
    }

    /// Goes to the page behind or ahead; the history moves when it
    /// arrives.
    fn step(&mut self, motion: Motion) {
        let entry = match motion {
            Motion::Back => self.back.last(),
            Motion::Forward => self.forward.last(),
            Motion::New | Motion::Reload => None,
        };
        if let Some(entry) = entry.cloned() {
            self.load(entry.url, entry.fragment, motion, entry.top);
        }
    }

    fn reload(&mut self) {
        if let Some(entry) = self.entry() {
            self.load(entry.url, entry.fragment, Motion::Reload, entry.top);
        }
    }

    fn stop(&mut self) -> bool {
        let stopped = self.loading.take().is_some();
        if stopped {
            self.message = Some("stopped".into());
            self.show_address();
        }
        stopped
    }

    /// The address row says where the page is, or where it is going.
    fn show_address(&mut self) {
        let (url, fragment) = match &self.loading {
            Some(loading) => (Some(&loading.url), &loading.fragment),
            None => (self.current.doc.url.as_ref(), &self.current.fragment),
        };
        let text = match (url, fragment) {
            (Some(url), Some(fragment)) => {
                format!("{}#{}", url.text(), document::visible(fragment))
            }
            (Some(url), None) => url.text(),
            (None, _) => String::new(),
        };
        if self.address.set_text(&text).is_err() {
            let _ = self
                .address
                .set_text(&url.map(Url::text).unwrap_or_default());
        }
    }

    // The page.

    /// Renders the page again when its width changed, keeping the focused
    /// link and what find marked, and keeps the view inside the page.
    fn relayout(&mut self) {
        let columns = self.columns();
        if self.current.rendered_for != Some(columns) {
            self.current.rendered = self.current.doc.render(columns);
            self.current.rendered_for = Some(columns);
            let view = &mut self.current.view;
            view.focus = view
                .focus
                .filter(|&link| link < self.current.rendered.links.len());
            if !view.hits.is_empty() || self.focus == Focus::Find {
                self.refind();
            }
        }
        self.scroll(0);
    }

    fn refind(&mut self) {
        let rows = self.rows();
        let query = self.query.text().to_string();
        self.current.view.find(&query, &self.current.rendered, rows);
        self.refresh_hover();
    }

    fn follow_link(&mut self, link: usize) {
        match self.current.rendered.target(link) {
            Some(Ok(reference)) => self.follow(reference),
            Some(Err(why)) => self.message = Some(format!("the link is not followed: {why}")),
            None => {}
        }
    }

    fn scroll(&mut self, delta: isize) {
        let rows = self.rows();
        self.current
            .view
            .scroll(delta, &self.current.rendered, rows);
        self.refresh_hover();
    }

    fn refresh_hover(&mut self) {
        self.hover = self.pointer.and_then(|(x, y)| self.link_at(x, y));
    }

    fn page_key(&mut self, chord: &str) {
        let rows = self.rows();
        let page = rows.saturating_sub(1).max(1) as isize;
        let view = &mut self.current.view;
        let rendered = &self.current.rendered;
        match chord {
            "Up" => self.scroll(-1),
            "Down" => self.scroll(1),
            "PageUp" | "S-space" => self.scroll(-page),
            "PageDown" | "space" => self.scroll(page),
            "Home" | "C-Home" => {
                view.home();
                self.refresh_hover();
            }
            "End" | "C-End" => {
                view.end(rendered, rows);
                self.refresh_hover();
            }
            "Tab" => {
                view.step_link(true, rendered, rows);
                self.refresh_hover();
            }
            "S-Tab" => {
                view.step_link(false, rendered, rows);
                self.refresh_hover();
            }
            "Return" => {
                if let Some(link) = view.focus {
                    self.follow_link(link);
                }
            }
            "n" => {
                view.step_hit(true, rows);
                self.refresh_hover();
            }
            "N" | "S-n" => {
                view.step_hit(false, rows);
                self.refresh_hover();
            }
            "/" => self.open_find(),
            "?" => self.show_keys = true,
            "Escape" if !self.stop() => {
                self.current.view.focus = None;
                self.current.view.hits.clear();
            }
            _ => {}
        }
    }

    fn open_find(&mut self) {
        self.focus = Focus::Find;
        let _ = self.query.act(Action::SelectAll);
    }

    fn copy(&mut self, clipboard: &mut dyn Clipboard) {
        let text = match self.focus {
            Focus::Address => self.address.copy().ok().flatten(),
            Focus::Find => self.query.copy().ok().flatten(),
            Focus::Page => None,
        };
        let text = text.or_else(|| {
            let link = self.current.view.focus?;
            Some(self.current.rendered.shown(link)?.into())
        });
        let text = text.or_else(|| Some(self.current.doc.url.as_ref()?.text().into()));
        if let Some(text) = text {
            self.message = Some(match clipboard.copy(text) {
                Ok(()) => "copied".into(),
                Err(why) => format!("not copied: {why}"),
            });
        }
    }

    /// An editing chord in `entry`; whether it was one.
    fn edit(entry: &mut EntryModel, chord: &str, clipboard: &mut dyn Clipboard) -> bool {
        let action = match chord {
            "space" => Some(Action::Insert(' ')),
            "C-v" => {
                let _ = clipboard.paste();
                return true;
            }
            "C-x" => {
                if let Ok(Some(text)) = entry.cut() {
                    let _ = clipboard.copy(text);
                }
                return true;
            }
            _ => Action::from_chord(chord),
        };
        match action {
            Some(action) => {
                let _ = entry.act(action);
                true
            }
            None => false,
        }
    }

    fn key(&mut self, chord: &str, clipboard: &mut dyn Clipboard) {
        match chord {
            "C-l" | "F6" => {
                self.focus = Focus::Address;
                let _ = self.address.act(Action::SelectAll);
                return;
            }
            "C-r" | "F5" => return self.reload(),
            "M-Left" => return self.step(Motion::Back),
            "M-Right" => return self.step(Motion::Forward),
            "C-q" | "C-w" => {
                self.quit = true;
                return;
            }
            "C-f" => return self.open_find(),
            "C-c" => return self.copy(clipboard),
            _ => {}
        }
        match self.focus {
            Focus::Address => match chord {
                "Return" => {
                    let typed = self.address.text().to_string();
                    self.go(&typed);
                }
                "Escape" => {
                    self.show_address();
                    self.focus = Focus::Page;
                }
                "Tab" | "Down" => self.focus = Focus::Page,
                _ => {
                    Self::edit(&mut self.address, chord, clipboard);
                }
            },
            Focus::Find => {
                let rows = self.rows();
                match chord {
                    "Return" => self.current.view.step_hit(true, rows),
                    "S-Return" => self.current.view.step_hit(false, rows),
                    "Escape" => self.focus = Focus::Page,
                    _ => {
                        if Self::edit(&mut self.query, chord, clipboard) {
                            self.refind();
                        }
                    }
                }
                self.refresh_hover();
            }
            Focus::Page => self.page_key(chord),
        }
    }

    /// A pasted text's first line into the focused row; on the page, into
    /// the address row, which takes the focus.
    fn paste(&mut self, text: &str) {
        let line = text.lines().next().unwrap_or_default();
        match self.focus {
            Focus::Find => {
                let _ = self.query.paste(line);
                self.refind();
            }
            Focus::Address => {
                let _ = self.address.paste(line);
            }
            Focus::Page => {
                self.focus = Focus::Address;
                let _ = self.address.act(Action::SelectAll);
                let _ = self.address.paste(line);
            }
        }
    }

    fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64) {
        if phase != PointerPhase::Press {
            return;
        }
        let address = self.address_rect();
        if address.contains(x, y) {
            self.focus = Focus::Address;
            if let Some(entry) = TextEntry::new(self.surface, address) {
                self.address.place(entry, x, y, false);
            }
            return;
        }
        if self.page_rect().contains(x, y) {
            self.focus = Focus::Page;
            if let Some(link) = self.link_at(x, y) {
                self.follow_link(link);
            }
        }
    }

    /// The link painted at a point: none in the inset, the cell the page
    /// does not show whole on the right or the row below the last whole.
    fn link_at(&self, x: i64, y: i64) -> Option<usize> {
        let rect = self.page_rect();
        let (cw, ch) = self.cell();
        if !rect.contains(x, y) || x < rect.x + cw {
            return None;
        }
        let column = usize::try_from((x - rect.x - cw) / cw).ok()?;
        let painted = usize::try_from(i64::from(rect.width) / cw - 1).unwrap_or(0);
        if column >= painted {
            return None;
        }
        let row = usize::try_from((y - rect.y) / ch).ok()?;
        self.current
            .view
            .link_at(&self.current.rendered, self.rows(), row, column)
    }

    /// What the status row says: what the last input made happen, else
    /// what loads, else where the link under the pointer or focused goes,
    /// else the page's status, place and notes.
    fn status_text(&self) -> String {
        if let Some(message) = &self.message {
            return message.clone();
        }
        if let Some(loading) = &self.loading {
            return format!("loading {}", loading.url.text());
        }
        let link = self.hover.or(self.current.view.focus);
        if let Some(shown) = link.and_then(|link| self.current.rendered.shown(link)) {
            return shown;
        }
        let doc = &self.current.doc;
        let rendered = &self.current.rendered;
        let mut parts = Vec::new();
        if doc.status != 0 && !(200..300).contains(&doc.status) {
            parts.push(format!("status {}", doc.status));
        }
        let lines = rendered.lines.len();
        if lines > self.rows() {
            let bottom = (self.current.view.top + self.rows()).min(lines);
            parts.push(format!("{bottom} of {lines} lines"));
        }
        if let Some(note) = &doc.note {
            parts.push(note.clone());
        }
        if rendered.cut {
            parts.push(format!(
                "only the first {} lines are shown",
                document::MAX_LINES
            ));
        }
        if doc.url.is_none() && lines <= 1 {
            parts.push("Ctrl+L for an address, F1 for the keys".into());
        }
        parts.join(" · ")
    }
}

fn push_bounded(list: &mut Vec<Entry>, entry: Entry) {
    if list.len() >= MAX_HISTORY {
        list.remove(0);
    }
    list.push(entry);
}

const PAGE_KEYS: &[(&str, &str)] = &[
    ("Tab, Shift+Tab", "next or previous link"),
    ("Return", "follow the focused link"),
    ("Space, PageDown, PageUp", "page down or up"),
    ("Home, End", "top or bottom"),
    ("/, Ctrl+F", "find; n and N next and previous"),
    ("Escape", "stop loading, else clear focus and find"),
];
const BROWSER_KEYS: &[(&str, &str)] = &[
    ("Ctrl+L, F6", "the address"),
    ("Alt+Left, Alt+Right", "back, forward"),
    ("Ctrl+R, F5", "reload"),
    ("Ctrl+C", "copy the link's or the page's address"),
    ("Ctrl+Q, Ctrl+W", "close"),
];

impl Handler for App {
    fn app_id(&self) -> &str {
        "td-web"
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) -> Flow {
        match input {
            Input::Key { chord, .. } => {
                self.message = None;
                self.key(chord, clipboard);
            }
            Input::Pointer { phase, x, y, .. } => {
                if phase == PointerPhase::Press {
                    self.message = None;
                }
                self.pointer(phase, x, y);
            }
            Input::Wheel { rows, .. } => {
                self.message = None;
                self.scroll(rows);
            }
            Input::Resize(surface) => {
                self.surface = surface;
                self.current.fitted = None;
                self.relayout();
            }
            Input::Paste(text) => {
                self.message = None;
                self.paste(text);
            }
            Input::Close => self.quit = true,
            _ => return self.flow(),
        }
        self.redraw = true;
        self.flow()
    }

    fn poll(&mut self, _now: u64) -> Flow {
        if self.loading.is_some() {
            self.settle();
            self.redraw = true;
        }
        self.flow()
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        if self.loading.is_some() {
            LOADING_WAIT_MS
        } else {
            IDLE_WAIT_MS
        }
    }

    fn needs_redraw(&self) -> bool {
        self.redraw
    }

    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        if surface != self.surface {
            self.surface = surface;
            self.current.fitted = None;
            self.relayout();
        }
        let bounds = surface.bounds();
        let mut sink = |draw| raster.draw(draw);
        fill(bounds, CHROME, bounds, &mut sink);
        let address = self.address_rect();
        if let Some(entry) = TextEntry::new(surface, address) {
            self.address.reveal(entry);
            let focused = self.focus == Focus::Address;
            let field = self
                .address
                .field("an address, or Ctrl+L", focused, focused);
            entry.emit(field, bounds, &mut sink);
        }
        let page = self.page_rect();
        match self.current.doc.picture() {
            Some(picture) => {
                fill(page, PAPER, bounds, &mut sink);
                let fitted = match self.current.fitted.take() {
                    Some(fitted) => Some(fitted),
                    None => {
                        let (width, height) =
                            picture.fit_size(page.width as usize, page.height as usize);
                        picture.fit(width, height)
                    }
                };
                if let Some(fitted) = &fitted {
                    raster.image(page, page, fitted);
                }
                self.current.fitted = fitted;
            }
            None => view::paint(
                &self.current.rendered,
                &self.current.view,
                self.hover,
                page,
                surface.scale,
                &mut sink,
            ),
        }
        let status = self.status();
        let mut sink = |draw| raster.draw(draw);
        if self.focus == Focus::Find {
            if let Some(entry) = TextEntry::new(surface, status.rect()) {
                self.query.reveal(entry);
                let field = self.query.field("find in the page", true, true);
                entry.emit(field, bounds, &mut sink);
            }
        } else {
            status.emit(self.status_text().chars(), bounds, &mut sink);
        }
        self.redraw = false;
        Ok(())
    }

    fn motion(&mut self, at: Option<(i64, i64)>) {
        self.pointer = at;
        let hover = self.hover;
        self.refresh_hover();
        if hover != self.hover {
            self.redraw = true;
        }
    }

    fn keys(&self) -> Vec<Section> {
        vec![
            Section::new("Page", PAGE_KEYS),
            Section::new("Browser", BROWSER_KEYS),
        ]
    }

    fn take_show_keys(&mut self) -> bool {
        std::mem::take(&mut self.show_keys)
    }
}

impl App {
    fn flow(&self) -> Flow {
        if self.quit {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }
}

/// Runs the browser in a window on the compositor the environment names,
/// at `start` when given one, until the window closes.
pub fn run(start: Option<String>) -> Result<(), String> {
    let endpoint = td_ui::wayland::endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )?;
    let stream = td_ui::wayland::connect(endpoint)?;
    let scale = td_ui::raster::Scale::new(1).map_err(|why| format!("{why:?}"))?;
    let surface = Surface::new(640, 480, scale).map_err(|why| format!("{why:?}"))?;
    let mut app = App::new(Arc::new(fetch::Service), surface)?;
    if let Some(start) = start {
        app.open(&start);
    }
    let typeface = td_ui::pinned_face::load_or_note(
        "td-web",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    td_ui::window::run(&mut app, stream, std::env::temp_dir(), typeface)
        .map_err(|why| why.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;
    use crate::fetch::tests::Table;
    use crate::fetch::Reply;
    use std::sync::Mutex;
    use td_ui::raster::Scale;

    #[derive(Default)]
    struct Board {
        copies: Vec<String>,
    }

    impl Clipboard for Board {
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
            self.copies.push(text.to_string());
            Ok(())
        }
        fn paste(&mut self) -> Result<(), td_ui::window::Refusal> {
            Ok(())
        }
    }

    /// Answers each request only when the test releases it.
    struct Held {
        table: Table,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl Fetcher for Held {
        fn get(&self, url: &str, public: bool) -> Result<Reply, String> {
            let _ = self.release.lock().unwrap().recv();
            self.table.get(url, public)
        }
    }

    fn surface(width: usize) -> Surface {
        Surface::new(width, 600, Scale::new(1).unwrap()).unwrap()
    }

    fn app_over(fetcher: Arc<dyn Fetcher>) -> App {
        App::new(fetcher, surface(800)).unwrap()
    }

    fn app(table: Table) -> App {
        app_over(Arc::new(table))
    }

    /// Waits for the fetch out to answer and takes it.
    fn settle(app: &mut App) {
        for _ in 0..1000 {
            app.poll(0);
            if app.loading.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the fetch did not answer");
    }

    /// Waits for every worker to end.
    fn idle(app: &App) {
        for _ in 0..1000 {
            if app.workers.load(Ordering::SeqCst) == 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("a worker did not end");
    }

    fn key(app: &mut App, chord: &str) {
        let mut board = Board::default();
        app.input(
            Input::Key {
                chord,
                repeat: false,
            },
            &mut board,
        );
    }

    fn type_address(app: &mut App, text: &str) {
        key(app, "C-l");
        for c in text.chars() {
            key(app, &c.to_string());
        }
        key(app, "Return");
    }

    fn typed(app: &mut App, text: &str) {
        type_address(app, text);
        settle(app);
    }

    fn site() -> Table {
        Table::default()
            .page(
                "https://example.org/",
                "text/html",
                "<title>Home</title><p><a href=\"/a\">to a</a> \
                 <a href=\"mailto:x@example.org\">mail</a></p>",
            )
            .page(
                "https://example.org/a",
                "text/html",
                "<title>A</title><p>page a</p>",
            )
            .page(
                "https://example.org/b",
                "text/html",
                "<title>B</title><p>page b</p>",
            )
            .page(
                "https://example.org/c",
                "text/html",
                "<title>C</title><p>page c</p>",
            )
    }

    fn history(app: &App) -> (Vec<String>, Vec<String>) {
        let paths = |list: &Vec<Entry>| list.iter().map(|entry| entry.url.text()).collect();
        (paths(&app.back), paths(&app.forward))
    }

    #[test]
    fn an_address_loads_and_titles_the_window() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        assert_eq!(app.title(), "Home - td-web");
        assert_eq!(app.address.text(), "https://example.org/");
        assert_eq!(app.current.rendered.links.len(), 2);
        assert_eq!(app.focus, Focus::Page);
    }

    #[test]
    fn a_link_is_followed_and_history_steps_back_and_forward() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        key(&mut app, "Tab");
        key(&mut app, "Return");
        settle(&mut app);
        assert_eq!(app.title(), "A - td-web");
        assert_eq!(app.back.len(), 1);
        key(&mut app, "M-Left");
        settle(&mut app);
        assert_eq!(app.title(), "Home - td-web");
        assert_eq!((app.back.len(), app.forward.len()), (0, 1));
        key(&mut app, "M-Right");
        settle(&mut app);
        assert_eq!(app.title(), "A - td-web");
        assert_eq!((app.back.len(), app.forward.len()), (1, 0));
        key(&mut app, "C-r");
        settle(&mut app);
        assert_eq!(app.back.len(), 1, "a reload is no step");
    }

    #[test]
    fn a_stopped_step_leaves_the_history_as_it_was() {
        let (release, held) = mpsc::channel();
        let mut app = app_over(Arc::new(Held {
            table: site(),
            release: Mutex::new(held),
        }));
        for page in ["example.org/a", "example.org/b", "example.org/c"] {
            type_address(&mut app, page);
            release.send(()).unwrap();
            settle(&mut app);
        }
        let before = history(&app);
        assert_eq!(before.0.len(), 2);
        key(&mut app, "M-Left");
        key(&mut app, "Escape");
        assert!(app.loading.is_none());
        assert_eq!(app.status_text(), "stopped");
        assert_eq!(history(&app), before);
        assert_eq!(app.title(), "C - td-web");
        // Its late answer is not taken.
        release.send(()).unwrap();
        idle(&app);
        app.poll(0);
        assert_eq!(app.title(), "C - td-web");
        assert_eq!(app.address.text(), "https://example.org/c");
        // Two quick steps back go one page back, as the second overtakes
        // the first, which never arrives.
        key(&mut app, "M-Left");
        key(&mut app, "M-Left");
        release.send(()).unwrap();
        release.send(()).unwrap();
        settle(&mut app);
        assert_eq!(app.title(), "B - td-web");
        assert_eq!(
            history(&app),
            (
                vec!["https://example.org/a".to_string()],
                vec!["https://example.org/c".to_string()]
            )
        );
    }

    #[test]
    fn past_the_workers_a_navigation_is_refused_and_the_history_kept() {
        let (release, held) = mpsc::channel();
        let mut app = app_over(Arc::new(Held {
            table: site(),
            release: Mutex::new(held),
        }));
        for _ in 0..MAX_WORKERS {
            type_address(&mut app, "example.org/a");
        }
        assert_eq!(app.workers.load(Ordering::SeqCst), MAX_WORKERS);
        type_address(&mut app, "example.org/b");
        assert!(
            app.status_text().contains("still loading"),
            "{}",
            app.status_text()
        );
        assert_eq!(
            app.loading.as_ref().unwrap().url.text(),
            "https://example.org/a"
        );
        for _ in 0..MAX_WORKERS {
            release.send(()).unwrap();
        }
        settle(&mut app);
        idle(&app);
        assert_eq!(app.title(), "A - td-web");
        assert_eq!(history(&app), (vec![], vec![]));
    }

    #[test]
    fn another_scheme_is_named_not_opened() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        key(&mut app, "Tab");
        key(&mut app, "Tab");
        assert_eq!(
            app.status_text(),
            "mailto:x@example.org",
            "the focused link"
        );
        key(&mut app, "Return");
        assert!(app.loading.is_none());
        assert!(
            app.status_text().contains("does not open mailto"),
            "{}",
            app.status_text()
        );
        key(&mut app, "Tab");
        assert_eq!(
            app.status_text(),
            "https://example.org/a",
            "said until the next input"
        );
    }

    #[test]
    fn a_failed_fetch_is_a_page_that_says_why() {
        let mut app = app(site());
        typed(&mut app, "example.org/missing");
        let text: String = app.current.rendered.lines[0]
            .iter()
            .map(|run| run.text.as_str())
            .collect();
        assert!(
            text.contains("no route to https://example.org/missing"),
            "{text}"
        );
        assert_eq!(app.address.text(), "https://example.org/missing");
        assert_eq!(app.back.len(), 0, "the start page is not kept");
    }

    #[test]
    fn a_redirect_elsewhere_is_fetched_public() {
        let table = Arc::new(
            site()
                .redirect("https://example.org/out", "https://other.example/")
                .page("https://other.example/", "text/plain", "there"),
        );
        let mut app = app_over(table.clone());
        typed(&mut app, "example.org/out");
        assert_eq!(app.address.text(), "https://other.example/");
        assert_eq!(
            table.reach(),
            [
                ("https://example.org/out".to_string(), false),
                ("https://other.example/".to_string(), true)
            ]
        );
    }

    #[test]
    fn find_marks_matches_through_a_resize_and_copy_takes_the_focused_link() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        key(&mut app, "/");
        for c in ["t", "o"] {
            key(&mut app, c);
        }
        assert_eq!(app.current.view.hits.len(), 1);
        key(&mut app, "Escape");
        key(&mut app, "Tab");
        let mut board = Board::default();
        app.input(Input::Resize(surface(400)), &mut board);
        assert_eq!(app.current.rendered_for, Some(app.columns()));
        assert_eq!(app.current.view.hits.len(), 1, "kept");
        assert_eq!(app.current.view.focus, Some(0), "kept");
        app.input(
            Input::Key {
                chord: "C-c",
                repeat: false,
            },
            &mut board,
        );
        assert_eq!(board.copies, ["https://example.org/a"]);
        // A paste into find finds.
        key(&mut app, "/");
        app.input(Input::Paste("mail\nignored"), &mut board);
        assert_eq!(app.query.text(), "mail");
        assert_eq!(app.current.view.hits.len(), 1);
    }

    #[test]
    fn a_click_on_a_link_follows_it_and_the_inset_is_no_link() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        let (cw, ch) = app.cell();
        let page = app.page_rect();
        let line = app.current.rendered.link_lines[0] as i64;
        let column = app.current.rendered.lines[line as usize]
            .iter()
            .take_while(|run| run.link != Some(0))
            .map(|run| run.text.chars().count())
            .sum::<usize>() as i64;
        assert_eq!(column, 0, "the page begins with the link");
        app.motion(Some((page.x + cw - 1, page.y + line * ch + 1)));
        assert_eq!(app.hover, None, "the inset");
        let (x, y) = (page.x + cw + column * cw + 1, page.y + line * ch + 1);
        app.motion(Some((x, y)));
        assert_eq!(app.status_text(), "https://example.org/a");
        let mut board = Board::default();
        app.input(
            Input::Pointer {
                phase: PointerPhase::Press,
                x,
                y,
                extend: false,
                follow: false,
            },
            &mut board,
        );
        settle(&mut app);
        assert_eq!(app.title(), "A - td-web");
        assert_eq!(app.hover, None, "nothing under the pointer on the new page");
    }

    #[test]
    fn hover_follows_the_page_under_a_still_pointer() {
        let mut lines = String::from("<p><a href=\"/a\">to a</a></p>");
        for n in 0..200 {
            lines.push_str(&format!("<p>line {n}</p>"));
        }
        let mut app = app(site().page("https://example.org/long", "text/html", &lines));
        typed(&mut app, "example.org/long");
        let (cw, _) = app.cell();
        let page = app.page_rect();
        app.motion(Some((page.x + cw + 1, page.y + 1)));
        assert_eq!(app.hover, Some(0));
        key(&mut app, "Down");
        assert_eq!(app.hover, None, "the link scrolled away");
        key(&mut app, "Up");
        assert_eq!(app.hover, Some(0));
        let mut board = Board::default();
        app.input(
            Input::Wheel {
                rows: 3,
                columns: 0,
            },
            &mut board,
        );
        assert_eq!(app.hover, None);
    }

    #[test]
    fn a_fragment_with_controls_is_shown_encoded() {
        let table = site().page(
            "https://example.org/f",
            "text/html",
            "<p><a href=\"/a#x\u{1}y\">go</a></p>",
        );
        let mut app = app(table);
        typed(&mut app, "example.org/f");
        key(&mut app, "Tab");
        key(&mut app, "Return");
        settle(&mut app);
        assert_eq!(app.address.text(), "https://example.org/a#x%01y");
    }
}
