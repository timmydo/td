//! The browser in its window (DESIGN.md §8): the address row, the page
//! and the status row; the history; and each navigation's fetch on a
//! thread of its own, which hands the reply to the one render thread
//! to be decoded, a page rendered into logical lines, and waits for it,
//! its document taken when the window next polls. Wrapping those lines
//! at the page's width in pixels is the window's.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;

use td_ui::chrome::{Status, TextEntry, ROW};
use td_ui::entry_model::{Action, EntryModel};
use td_ui::keys::Section;
use td_ui::picture::Fitted;
use td_ui::prose::{Faces, Metrics};
use td_ui::raster::{fill, Raster, Rect, Scale, Surface, TextFamily, CHROME, PAPER};
use td_ui::window::{Clipboard, Flow, Handler, Input, PointerPhase};
use td_ui::CELL_WIDTH;
use td_url::{Reference, Url};

use crate::document::{self, Document, Place, Rendered, Renderer};
use crate::fetch::{self, Fetcher};
use crate::view::{self, Pane, View};

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
/// The narrowest the page is wrapped, in ems of the prose size at the
/// scale: a narrower pane shows each line's start, cut at its edge as a
/// preformatted line is, rather than a line for every few glyphs.
pub const MIN_WRAP_EMS: i64 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Address,
    Page,
    Find,
}

/// A place in the history: the URL, the fragment it named and how far
/// down it was read, as a place in its logical lines, so a page wrapped
/// otherwise when it is back opens at the same text.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    url: Url,
    fragment: Option<String>,
    top: Place,
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

/// A fetch out on its thread. Dropping it, as stopping, overtaking or
/// taking its answer does, tells its worker to skip any rendering or
/// decoding it has not begun.
struct Loading {
    url: Url,
    fragment: Option<String>,
    motion: Motion,
    /// Where to show first once it is in, for a history step.
    top: Place,
    answer: Receiver<Option<Arrival>>,
    stopped: Arc<AtomicBool>,
}

impl Drop for Loading {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

/// What a rendering was wrapped for: the width in pixels, the scale and
/// whether proportional faces measured it, and the body text's family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Layout {
    width: u32,
    scale: usize,
    proportional: bool,
    body: TextFamily,
}

/// The page shown, and its rendering for the layout it was made for.
struct Current {
    doc: Document,
    fragment: Option<String>,
    view: View,
    rendered: Rendered,
    rendered_for: Option<Layout>,
    fitted: Option<Fitted>,
}

pub struct App {
    fetcher: Arc<dyn Fetcher>,
    /// Where each fetch's reply is made into its document.
    renderer: Renderer,
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
    /// A clone of the proportional faces the window draws prose with,
    /// none drawing it on the cell, and what lays the page out with them
    /// at the surface's scale: the same measurements the window's raster
    /// draws with, made outside a paint.
    faces: Option<Faces>,
    metrics: Metrics,
    /// Whether body text is in Serif rather than Sans.
    serif: bool,
}

/// Counts a worker until its thread ends, however it ends.
struct Worker(Arc<AtomicUsize>);

impl Drop for Worker {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Fetches `url` on a worker's thread and has `renderer` make its
/// document; none once its navigation is `stopped` before the document's
/// rendering or decoding begins.
fn arrive(
    fetcher: &dyn Fetcher,
    renderer: &Renderer,
    url: Url,
    fragment: Option<String>,
    stopped: &Arc<AtomicBool>,
) -> Option<Arrival> {
    match fetch::navigate(fetcher, url.clone()) {
        Ok(loaded) => {
            let content_type = loaded.reply.header("content-type").map(str::to_string);
            let doc = renderer.document(
                loaded.url,
                loaded.reply.status,
                content_type.as_deref(),
                loaded.reply.body,
                stopped,
            )?;
            Some((doc, loaded.fragment.or(fragment)))
        }
        Err(why) => Some((Document::error(Some(url), &why), fragment)),
    }
}

impl App {
    /// A browser over `fetcher` on `surface`, its page laid out with
    /// `faces`, the ones its window draws prose with; none lays it on the
    /// cell.
    pub fn new(
        fetcher: Arc<dyn Fetcher>,
        surface: Surface,
        faces: Option<Faces>,
    ) -> Result<Self, String> {
        let entry = |limit| EntryModel::new(limit).map_err(|why| format!("{why:?}"));
        let metrics = Metrics::new(faces.as_ref(), surface.scale);
        Ok(Self {
            fetcher,
            renderer: Renderer::start()?,
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
            faces,
            metrics,
            serif: false,
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

    /// The inset each side of the page's lines: a cell's width.
    fn inset(&self) -> i64 {
        CELL_WIDTH as i64 * self.scale()
    }

    /// The page as its lines are shown in it, in the line box they are
    /// measured with.
    fn pane(&self) -> Pane {
        Pane {
            rect: self.page_rect(),
            inset: self.inset(),
            line: self.metrics.line(),
            scale: self.scale(),
        }
    }

    /// What the page is wrapped for now: the page's width less an inset
    /// each side, at least `MIN_WRAP_EMS`.
    fn layout(&self) -> Layout {
        let width = i64::from(self.page_rect().width) - 2 * self.inset();
        let least = MIN_WRAP_EMS * i64::from(td_ui::prose::SIZE) * self.scale();
        Layout {
            width: u32::try_from(width.max(least)).unwrap_or(1),
            scale: self.metrics.scale().value(),
            proportional: self.metrics.proportional(),
            body: if self.serif {
                TextFamily::Serif
            } else {
                TextFamily::Sans
            },
        }
    }

    /// The page's lines shown whole, at least one.
    fn rows(&self) -> usize {
        self.pane().rows()
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
                    self.load(url, fragment, Motion::New, Place::default());
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

    fn load(&mut self, url: Url, fragment: Option<String>, motion: Motion, top: Place) {
        if self.workers.load(Ordering::SeqCst) >= MAX_WORKERS {
            self.message = Some("earlier pages are still loading; try again shortly".into());
            return;
        }
        let (send, answer) = mpsc::channel();
        let fetcher = Arc::clone(&self.fetcher);
        let renderer = self.renderer.clone();
        let (target, named) = (url.clone(), fragment.clone());
        let stopped = Arc::new(AtomicBool::new(false));
        let told = Arc::clone(&stopped);
        self.workers.fetch_add(1, Ordering::SeqCst);
        let worker = Worker(Arc::clone(&self.workers));
        let spawned = std::thread::Builder::new()
            .name("td-web fetch".into())
            .spawn(move || {
                let _worker = worker;
                let _ = send.send(arrive(fetcher.as_ref(), &renderer, target, named, &told));
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
            stopped,
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
            Ok(Some(arrival)) => arrival,
            Err(TryRecvError::Empty) => return,
            Ok(None) | Err(TryRecvError::Disconnected) => (
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

    fn install(&mut self, doc: Document, fragment: Option<String>, top: Place) {
        self.title = match (&doc.title, &doc.url) {
            (Some(title), _) => format!("{title} - td-web"),
            (None, Some(url)) => format!("{} - td-web", url.text()),
            (None, None) => "td-web".into(),
        };
        self.current = Current {
            doc,
            fragment,
            view: View::default(),
            rendered: Rendered::default(),
            rendered_for: None,
            fitted: None,
        };
        self.relayout();
        self.current.view.top = self.current.rendered.line_at(top);
        self.scroll(0);
        self.show_address();
    }

    /// The current page as a history entry; none for the start page.
    fn entry(&self) -> Option<Entry> {
        Some(Entry {
            url: self.current.doc.url.clone()?,
            fragment: self.current.fragment.clone(),
            top: self.current.rendered.place(self.current.view.top),
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

    /// Wraps the page again when its width, scale or body face changed,
    /// keeping the text at the top, the focused link and find's marks,
    /// made again on the new lines without moving to a match, and keeps
    /// the view inside the page. The old rendering is gone before the new
    /// one is made.
    fn relayout(&mut self) {
        if self.metrics.scale() != self.surface.scale {
            self.metrics = Metrics::new(self.faces.as_ref(), self.surface.scale);
        }
        let layout = self.layout();
        if self.current.rendered_for != Some(layout) {
            let old = std::mem::take(&mut self.current.rendered);
            let top = old.place(self.current.view.top);
            drop(old);
            self.current.rendered =
                self.current
                    .doc
                    .render(&mut self.metrics, layout.width, layout.body);
            self.current.rendered_for = Some(layout);
            let rendered = &self.current.rendered;
            let view = &mut self.current.view;
            view.top = rendered.line_at(top);
            view.focus = view.focus.filter(|&link| link < rendered.page.links.len());
            if !view.hits.is_empty() || self.focus == Focus::Find {
                let query = self.query.text().to_string();
                self.current.view.remark(&query, &self.current.rendered);
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
        match self.current.rendered.page.target(link) {
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

    /// Sets body text in the other of Sans and Serif, for this run only.
    fn toggle_body(&mut self) {
        self.serif = !self.serif;
        self.relayout();
        let family = if self.serif { "Serif" } else { "Sans" };
        self.message = Some(if self.metrics.proportional() {
            format!("body text in Noto {family}")
        } else {
            format!("body text in Noto {family}, which is not loaded: the page draws on the cell")
        });
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
            Some(self.current.rendered.page.shown(link)?.into())
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
            "F8" => return self.toggle_body(),
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

    /// The link painted at a point, by the line it is on and the piece
    /// under it: none in the inset or the line below the last whole.
    fn link_at(&self, x: i64, y: i64) -> Option<usize> {
        self.current
            .view
            .link_at(&self.current.rendered, &self.pane(), x, y)
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
        if let Some(shown) = link.and_then(|link| self.current.rendered.page.shown(link)) {
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
        if rendered.page.unrendered {
            parts.push("the rest is too large to render".to_string());
        }
        if rendered.page.trimmed {
            parts.push(format!(
                "unwrapped lines are cut at {} characters",
                document::MAX_PRE_CHARS
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
    ("F8", "body text in Sans or Serif"),
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
                &self.pane(),
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
    let scale = Scale::new(1).map_err(|why| format!("{why:?}"))?;
    let surface = Surface::new(640, 480, scale).map_err(|why| format!("{why:?}"))?;
    // The monospace face, and the proportional faces sharing its bytes;
    // either missing is said once and drawn without.
    let setting = std::env::var_os(td_ui::pinned_face::SETTING);
    let typeface = td_ui::pinned_face::load_or_note("td-web", setting.as_deref());
    let prose = td_ui::pinned_face::prose_or_note("td-web", setting.as_deref(), typeface.as_ref());
    let mut app = App::new(Arc::new(fetch::Service), surface, prose.clone())?;
    if let Some(start) = start {
        app.open(&start);
    }
    td_ui::window::run_with_prose(&mut app, stream, std::env::temp_dir(), typeface, prose)
        .map_err(|why| why.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;
    use crate::fetch::tests::Table;
    use crate::fetch::Reply;
    use std::sync::Mutex;

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
        App::new(fetcher, surface(800), None).unwrap()
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

    /// The text a wrapped line shows, from its glyphs.
    fn shown(line: &td_ui::prose::Line) -> String {
        line.glyphs.iter().map(|glyph| glyph.scalar).collect()
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
        assert_eq!(app.current.rendered.page.links.len(), 2);
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
        let text = shown(&app.current.rendered.lines[0].line);
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
        assert_eq!(app.current.rendered_for, Some(app.layout()));
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
        let (inset, height) = (app.inset(), i64::from(app.metrics.line().height));
        let page = app.page_rect();
        let line = app.current.rendered.link_lines[0];
        let left = app.current.rendered.lines[line]
            .line
            .pieces
            .iter()
            .find(|piece| piece.tag == 0)
            .unwrap()
            .left;
        assert_eq!(left, 0, "the page begins with the link");
        let y = page.y + line as i64 * height + 1;
        app.motion(Some((page.x + inset - 1, y)));
        assert_eq!(app.hover, None, "the inset");
        let x = page.x + inset + i64::from(left) + 1;
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
        let page = app.page_rect();
        app.motion(Some((page.x + app.inset() + 1, page.y + 1)));
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

    #[test]
    fn f8_sets_body_text_in_serif_and_back_and_is_listed() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        let family = |app: &App| app.current.rendered.lines[0].line.pieces[0].style.family;
        assert_eq!(family(&app), TextFamily::Sans);
        key(&mut app, "F8");
        assert_eq!(family(&app), TextFamily::Serif);
        assert_eq!(app.current.rendered_for, Some(app.layout()));
        assert!(
            app.status_text().contains("Serif") && app.status_text().contains("not loaded"),
            "{}",
            app.status_text()
        );
        key(&mut app, "F8");
        assert_eq!(family(&app), TextFamily::Sans);
        assert!(BROWSER_KEYS.iter().any(|(keys, _)| *keys == "F8"));
    }

    /// The words a line shows.
    fn words(app: &App, line: usize) -> String {
        shown(&app.current.rendered.lines[line].line)
    }

    /// A page that is one logical line of numbered words, and an app
    /// scrolled into it.
    fn one_paragraph() -> App {
        let words: Vec<String> = (0..2000).map(|n| format!("w{n}")).collect();
        let page = format!("<div><a href=\"/a\">to a</a> {}</div>", words.join(" "));
        let mut app = app(site().page("https://example.org/p", "text/html", &page));
        typed(&mut app, "example.org/p");
        for _ in 0..30 {
            key(&mut app, "Down");
        }
        app
    }

    #[test]
    fn a_resize_or_f8_keeps_the_place_inside_a_logical_line() {
        let mut app = one_paragraph();
        assert_eq!(app.current.rendered.page.lines.len(), 1);
        // Each time the line at the top holds the word the last top
        // began with.
        let first = |app: &App| {
            let top = app.current.view.top;
            words(app, top).split(' ').next().unwrap().to_string()
        };
        let mut board = Board::default();
        for change in [Some(surface(300)), Some(surface(800)), None] {
            let word = first(&app);
            match change {
                Some(surface) => {
                    app.input(Input::Resize(surface), &mut board);
                }
                None => key(&mut app, "F8"),
            }
            let top = app.current.view.top;
            assert!(top > 0, "not the paragraph's start");
            let shown = words(&app, top);
            assert!(shown.split(' ').any(|w| w == word), "{word} in {shown}");
        }
    }

    #[test]
    fn back_after_a_resize_opens_at_the_same_text() {
        let mut app = one_paragraph();
        let mut board = Board::default();
        app.input(Input::Resize(surface(300)), &mut board);
        let top = app.current.view.top;
        let word = words(&app, top).split(' ').next().unwrap().to_string();
        typed(&mut app, "example.org/a");
        app.input(Input::Resize(surface(800)), &mut board);
        key(&mut app, "M-Left");
        settle(&mut app);
        let top = app.current.view.top;
        assert!(top > 0);
        let shown = words(&app, top);
        assert!(shown.split(' ').any(|w| w == word), "{word} in {shown}");
    }

    #[test]
    fn a_relayout_marks_matches_again_without_moving_to_one() {
        let mut lines = String::new();
        for n in 0..200 {
            lines.push_str(&format!("<p>line {n}</p>"));
        }
        let mut app = app(site().page("https://example.org/long", "text/html", &lines));
        typed(&mut app, "example.org/long");
        key(&mut app, "/");
        for c in "line 15".chars() {
            let chord = if c == ' ' {
                "space".to_string()
            } else {
                c.to_string()
            };
            key(&mut app, &chord);
        }
        key(&mut app, "Return");
        assert!(app.current.view.top > 0, "find moved to a match");
        let at = app.current.view.at;
        let hits = app.current.view.hits.len();
        key(&mut app, "Escape");
        key(&mut app, "Home");
        let mut board = Board::default();
        app.input(Input::Resize(surface(400)), &mut board);
        assert_eq!(app.current.view.top, 0, "not moved to the match");
        assert_eq!(app.current.view.hits.len(), hits);
        assert_eq!(app.current.view.at, at);
    }

    #[test]
    fn a_stopped_or_overtaken_navigation_tells_its_worker() {
        let (release, held) = mpsc::channel();
        let mut app = app_over(Arc::new(Held {
            table: site(),
            release: Mutex::new(held),
        }));
        type_address(&mut app, "example.org/a");
        let first = Arc::clone(&app.loading.as_ref().unwrap().stopped);
        type_address(&mut app, "example.org/b");
        assert!(first.load(Ordering::SeqCst), "overtaken");
        let second = Arc::clone(&app.loading.as_ref().unwrap().stopped);
        assert!(!second.load(Ordering::SeqCst));
        key(&mut app, "Escape");
        assert!(second.load(Ordering::SeqCst), "stopped");
        release.send(()).unwrap();
        release.send(()).unwrap();
        idle(&app);
    }

    #[test]
    fn a_narrow_pane_wraps_at_the_least_width_and_cuts_lines_at_its_edge() {
        let mut app = one_paragraph();
        let mut board = Board::default();
        app.input(Input::Resize(surface(40)), &mut board);
        let least = MIN_WRAP_EMS * i64::from(td_ui::prose::SIZE);
        assert_eq!(i64::from(app.layout().width), least);
        let pane = app.pane();
        let edge = pane.rect.x + i64::from(pane.rect.width);
        let left = pane.rect.x + pane.inset;
        let laid = &app.current.rendered.lines;
        assert!(
            laid.iter()
                .any(|wrapped| left + i64::from(wrapped.line.width) > edge),
            "lines wider than the pane"
        );
        let mut pens = Vec::new();
        view::paint(
            &app.current.rendered,
            &app.current.view,
            None,
            &pane,
            &mut |draw| {
                if let td_ui::raster::Primitive::Glyph { x, .. } = draw.primitive {
                    pens.push(x);
                }
            },
        );
        assert!(!pens.is_empty());
        let overhang = i64::from(td_ui::prose::OVERHANG);
        assert!(pens.iter().all(|&x| x < edge + overhang), "{pens:?}");
    }

    #[test]
    fn a_resize_keeps_the_line_at_the_top() {
        let mut page = String::new();
        for n in 0..100 {
            page.push_str(&format!("<p>paragraph {n} has words enough to wrap</p>"));
        }
        let mut app = app(site().page("https://example.org/p", "text/html", &page));
        typed(&mut app, "example.org/p");
        for _ in 0..41 {
            key(&mut app, "Down");
        }
        let logical = |app: &App| app.current.rendered.lines[app.current.view.top].logical;
        assert_eq!(logical(&app), 41);
        let wide = app.current.rendered.lines.len();
        let mut board = Board::default();
        app.input(Input::Resize(surface(200)), &mut board);
        assert!(app.current.rendered.lines.len() > wide);
        assert_eq!(logical(&app), 41);
        let top = app.current.view.top;
        assert_eq!(
            app.current.rendered.lines[top - 1].logical,
            40,
            "its first line"
        );
    }

    /// The pinned faces' directories for `family`: the image's and the
    /// XDG data home's, never a host's packaged faces.
    fn pinned(family: td_ui::face_file::Family) -> Vec<td_ui::face_file::Place> {
        family
            .host_places()
            .into_iter()
            .filter(|place| place.depth == 0)
            .collect()
    }

    // Over the pinned faces, read only where ./install-fonts and the
    // image put them; it fails without them and is ignored unless asked
    // for: `XDG_DATA_HOME=<an install> cargo test -- --ignored`.
    #[test]
    #[ignore = "needs the pinned faces: ./install-fonts, or XDG_DATA_HOME at an install"]
    fn over_the_pinned_faces_a_page_is_laid_hit_found_and_drawn_by_pixel() {
        use td_ui::face_file::Family;
        let mono = td_ui::pinned_face::load_in(&pinned(Family::MONO))
            .unwrap_or_else(|why| panic!("the pinned monospace face is not installed: {why}"));
        let faces = td_ui::pinned_face::prose_in(&pinned(Family::SANS))
            .unwrap_or_else(|why| panic!("the pinned prose faces are not installed: {why}"))
            .with_mono(&mono)
            .unwrap();
        let table = site().page(
            "https://example.org/p",
            "text/html",
            "<p>iiii mmmm <a href=\"/a\">a link</a> after</p><pre>i  m\nmm i</pre>",
        );
        let mut app = App::new(Arc::new(table), surface(800), Some(faces.clone())).unwrap();
        typed(&mut app, "example.org/p");
        assert!(app.metrics.proportional());
        assert_eq!(app.metrics.line().height, 22);
        let rendered = app.current.rendered.clone();
        let first = &rendered.lines[0].line;
        assert_eq!(shown(first), "iiii mmmm a link after");
        let advance = |at: usize| first.glyphs[at + 1].x - first.glyphs[at].x;
        assert!(
            advance(0) * 2 < advance(5),
            "i {} m {}",
            advance(0),
            advance(5)
        );
        // The pointer over the link's piece names it; past it, nothing.
        // td-html may give one link several runs: its extent is theirs.
        let link = first.pieces.iter().filter(|piece| piece.tag == 0);
        let start = link.clone().map(|piece| piece.left).min().unwrap();
        let end = link.map(|piece| piece.right).max().unwrap();
        let page = app.page_rect();
        let left = page.x + app.inset();
        app.motion(Some((left + i64::from(start + end) / 2, page.y + 1)));
        assert_eq!(app.status_text(), "https://example.org/a");
        app.motion(Some((left + i64::from(end) + 2, page.y + 1)));
        assert_eq!(app.hover, None);
        // A match spans its glyphs' pixels.
        key(&mut app, "/");
        for c in "after".chars() {
            key(&mut app, &c.to_string());
        }
        let hit = app.current.view.hits[0];
        assert_eq!((hit.line, hit.left), (0, first.glyphs[17].x));
        assert_eq!(hit.right, first.width);
        key(&mut app, "Escape");
        // A preformatted line is in the monospace face, one advance a
        // glyph, and not wrapped.
        let pre = &rendered.lines[2].line;
        assert!(pre
            .pieces
            .iter()
            .all(|p| p.style.family == TextFamily::Mono));
        let mut mono = |scalar| {
            app.metrics
                .advance(TextFamily::Mono, td_ui::atlas::Style::Regular, scalar)
        };
        assert_eq!(mono('i'), mono('m'));
        // Pens are the advances summed and rounded: a pixel apart at most.
        let steps: Vec<u32> = pre.glyphs.windows(2).map(|w| w[1].x - w[0].x).collect();
        let (low, high) = (steps.iter().min().unwrap(), steps.iter().max().unwrap());
        assert!(high - low <= 1, "{steps:?}");
        // Serif measures otherwise than Sans.
        key(&mut app, "F8");
        let serif = &app.current.rendered.lines[0].line;
        assert_eq!(serif.pieces[0].style.family, TextFamily::Serif);
        assert_ne!(serif.width, first.width);
        // And the window's raster, lent the same faces, draws it.
        let mut faces = faces;
        let font = td_ui::font::pinned().unwrap();
        let surface = app.surface;
        let mut pixels = vec![0u8; surface.width * surface.height * 4];
        {
            let mut raster = Raster::new(&mut pixels, &font, surface, surface.width * 4)
                .unwrap()
                .with_prose(Some(&mut faces));
            app.paint(&mut raster, surface).unwrap();
        }
        let row = |y: i64| {
            let start = (y as usize * surface.width + left as usize) * 4;
            pixels[start..start + 200 * 4].to_vec()
        };
        let ink = (page.y..page.y + 22).any(|y| {
            let row = row(y);
            row.chunks(4).any(|pixel| pixel != &row[..4])
        });
        assert!(ink, "the first line is drawn");
    }
}
