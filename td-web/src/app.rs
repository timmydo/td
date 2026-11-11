//! The browser in its window (DESIGN.md §8): the address row, the page
//! and the status row; the history; and each navigation's fetch on a
//! thread of its own, which hands the reply to the one render thread
//! to be decoded, a page rendered into logical lines, and waits for it,
//! its document taken when the window next polls. Wrapping those lines
//! at the page's width in pixels, the ones near those shown, is the
//! window's.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Instant;

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
use crate::view::{self, Pane, Selection, View};

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
/// The most text a copy takes, in bytes: the clipboard's own ceiling. A
/// selection past it is not copied, and the status row says so.
pub const MAX_COPY: usize = td_ui::clipboard::MAX_BYTES;
/// How far, in font pixels each way, the pointer may move between a press
/// and its release for the two to be a click, and a second press lie from
/// a click to make the pair a double click, as in td-ui's message list.
const NEAR: i64 = 4;
/// How long after a click's release a press makes a double or a triple
/// click, as in td-ui's message list.
const MULTI_CLICK_MS: u64 = td_ui::messages::MULTI_CLICK_MS;
/// While a selecting drag holds the pointer above or below the page, the
/// page scrolls once each `DRAG_SCROLL_MS`, a line for each line's height
/// the pointer is past the edge, at most `DRAG_SCROLL_LINES`.
const DRAG_SCROLL_MS: u64 = 50;
const DRAG_SCROLL_LINES: i64 = 4;

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

/// What a press of the button on the page began, until its release.
#[derive(Clone, Copy, Debug)]
struct Press {
    /// Where it was pressed, and where the pointer is now.
    from: (i64, i64),
    at: (i64, i64),
    /// Where a drag selects from.
    anchor: Place,
    /// The link under the press, followed by a click.
    link: Option<usize>,
    /// One for a press, two for a double click's, three for a triple's.
    count: u8,
    /// Whether it selects as the pointer moves: once it has moved further
    /// than `NEAR` from where it began, or from a Shift press at once. A
    /// double or a triple click's selection is not moved by a drag.
    selecting: bool,
    /// Whether the pointer has moved from where it was pressed: only a
    /// drag scrolls past the page's edge, not a Shift press on its last
    /// line, nor a move that leaves the pointer there.
    moved: bool,
}

/// A click: when it was released, where, and which of a run it was.
#[derive(Clone, Copy, Debug)]
struct Click {
    released: u64,
    at: (i64, i64),
    count: u8,
}

/// Milliseconds since the window began, for telling a double click from
/// two clicks: read at each press and release, since a pointer input
/// carries no time and is handled before the turn's `poll` is told one.
struct Clock {
    epoch: Instant,
    #[cfg(test)]
    fixed: Option<u64>,
}

impl Clock {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            #[cfg(test)]
            fixed: None,
        }
    }

    fn now(&self) -> u64 {
        #[cfg(test)]
        if let Some(fixed) = self.fixed {
            return fixed;
        }
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// The page shown, and its rendering for the layout it was made for.
struct Current {
    doc: Document,
    fragment: Option<String>,
    view: View,
    rendered: Rendered,
    rendered_for: Option<Layout>,
    fitted: PictureFit,
}

#[derive(Default)]
enum PictureFit {
    #[default]
    Empty,
    Ready(Fitted),
    Failed,
}
impl PictureFit {
    fn get_or_fit(&mut self, fit: impl FnOnce() -> Option<Fitted>) -> Option<&Fitted> {
        if matches!(self, Self::Empty) {
            *self = fit().map_or(Self::Failed, Self::Ready);
        }
        match self {
            Self::Ready(image) => Some(image),
            _ => None,
        }
    }
}

fn picture_notice(
    surface: Surface,
    page: Rect,
    bounds: Rect,
    sink: &mut dyn FnMut(td_ui::raster::Draw),
) {
    fill(page, PAPER, bounds, sink);
    td_ui::raster::text_run(
        surface.scale,
        "Picture unavailable: memory or decode budget exhausted.".chars(),
        (page.x, page.y),
        page,
        td_ui::raster::GlyphStyle::regular(td_ui::raster::INK, PAPER),
        bounds,
        sink,
    );
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
    /// The press on the page being held, and the click before it.
    press: Option<Press>,
    click: Option<Click>,
    clock: Clock,
    /// When a drag past the page's edge last scrolled it.
    drag_scrolled: Option<u64>,
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
                fitted: PictureFit::Empty,
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
            press: None,
            click: None,
            clock: Clock::new(),
            drag_scrolled: None,
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
        // A press on the page before it is not on this one.
        self.press = None;
        self.click = None;
        self.current = Current {
            doc,
            fragment,
            view: View::default(),
            rendered: Rendered::default(),
            rendered_for: None,
            fitted: PictureFit::Empty,
        };
        self.relayout();
        self.current.view.top = self.current.rendered.snap(&mut self.metrics, top);
        self.scroll(0);
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

    /// Wraps the page again when its width, scale or body face changed,
    /// keeping the text at the top, the focused link and find's marks,
    /// made again on the new lines without moving to a match, and keeps
    /// the view inside the page. The old lines are gone before the new
    /// ones are laid.
    fn relayout(&mut self) {
        if self.metrics.scale() != self.surface.scale {
            self.metrics = Metrics::new(self.faces.as_ref(), self.surface.scale);
        }
        let layout = self.layout();
        if self.current.rendered_for != Some(layout) {
            let top = self.current.view.top;
            self.current.rendered = Rendered::default();
            self.current.rendered = self.current.doc.render(layout.width, layout.body);
            self.current.rendered_for = Some(layout);
            let rendered = &mut self.current.rendered;
            let view = &mut self.current.view;
            view.top = rendered.snap(&mut self.metrics, top);
            view.focus = view.focus.filter(|&link| link < rendered.page.links.len());
            if !view.hits.is_empty() || self.focus == Focus::Find {
                let query = self.query.text().to_string();
                view.remark(&query, rendered, &mut self.metrics);
            }
        }
        self.scroll(0);
    }

    fn refind(&mut self) {
        let rows = self.rows();
        let query = self.query.text().to_string();
        let current = &mut self.current;
        current
            .view
            .find(&query, &mut current.rendered, &mut self.metrics, rows);
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
        let current = &mut self.current;
        current
            .view
            .scroll(delta, &mut current.rendered, &mut self.metrics, rows);
        self.refresh_hover();
    }

    fn refresh_hover(&mut self) {
        self.hover = self.pointer.and_then(|(x, y)| self.link_at(x, y));
    }

    fn page_key(&mut self, chord: &str) {
        let rows = self.rows();
        let page = rows.saturating_sub(1).max(1) as isize;
        let metrics = &mut self.metrics;
        let Current { view, rendered, .. } = &mut self.current;
        match chord {
            "Up" => self.scroll(-1),
            "Down" => self.scroll(1),
            "PageUp" | "S-space" => self.scroll(-page),
            "PageDown" | "space" => self.scroll(page),
            "Home" | "C-Home" => {
                view.home(rendered, metrics, rows);
                self.refresh_hover();
            }
            "End" | "C-End" => {
                view.end(rendered, metrics, rows);
                self.refresh_hover();
            }
            "Tab" => {
                view.step_link(true, rendered, metrics, rows);
                self.refresh_hover();
            }
            "S-Tab" => {
                view.step_link(false, rendered, metrics, rows);
                self.refresh_hover();
            }
            "Return" => {
                if let Some(link) = view.focus {
                    self.follow_link(link);
                }
            }
            "n" => {
                view.step_hit(true, rendered, metrics, rows);
                self.refresh_hover();
            }
            "N" | "S-n" => {
                view.step_hit(false, rendered, metrics, rows);
                self.refresh_hover();
            }
            "/" => self.open_find(),
            "?" => self.show_keys = true,
            "Escape" if !self.stop() => {
                self.current.view.focus = None;
                self.current.view.hits.clear();
                self.current.view.selection = None;
            }
            "C-a" => {
                let page = &self.current.rendered.page;
                let end = page.line_end(page.lines.len().saturating_sub(1));
                self.current.view.select(Some(Selection {
                    anchor: Place::default(),
                    focus: end,
                }));
            }
            "S-Left" | "S-Right" | "C-S-Left" | "C-S-Right" | "S-Up" | "S-Down" => {
                self.extend_by(chord);
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

    /// Moves the selection's focus by a keyboard chord: a character, a
    /// word or a logical line, from where it is; with nothing selected,
    /// from the focused link's start, else the first line shown's. The
    /// page scrolls as little as shows where it went.
    fn extend_by(&mut self, chord: &str) {
        let rows = self.rows();
        let Current { view, rendered, .. } = &mut self.current;
        let page = Arc::clone(&rendered.page);
        if page.lines.is_empty() {
            return;
        }
        let selection = view.selection.unwrap_or_else(|| {
            let start = view
                .focus
                .and_then(|link| page.link_place(link))
                .unwrap_or(view.top);
            Selection {
                anchor: start,
                focus: start,
            }
        });
        let at = selection.focus;
        let focus = match chord {
            "S-Left" => page.before_char(at),
            "S-Right" => page.after_char(at),
            "C-S-Left" => page.word_before(at),
            "C-S-Right" => page.word_after(at),
            "S-Up" => page.line_up(at),
            _ => page.line_down(at),
        };
        view.select(Some(Selection { focus, ..selection }));
        view.reveal(focus, rendered, &mut self.metrics, rows);
        self.refresh_hover();
    }

    /// The page's selected text, when any is selected: `Err` past
    /// `MAX_COPY`.
    fn selected_text(&self) -> Option<Result<String, ()>> {
        let (start, end) = self.current.view.selection?.range()?;
        let page = &self.current.rendered.page;
        match page.text(start, end, MAX_COPY) {
            Some(text) if text.is_empty() => None,
            Some(text) => Some(Ok(text)),
            None => Some(Err(())),
        }
    }

    /// Copies, first that applies: the address or find row's selection
    /// while it has focus, the page's selection, the focused link's URL,
    /// the page's.
    fn copy(&mut self, clipboard: &mut dyn Clipboard) {
        let text = match self.focus {
            Focus::Address => self.address.copy().ok().flatten(),
            Focus::Find => self.query.copy().ok().flatten(),
            Focus::Page => None,
        };
        let text = match text {
            Some(text) => Some(text),
            None => match self.selected_text() {
                Some(Ok(text)) => Some(text.into()),
                Some(Err(())) => {
                    self.message = Some(format!(
                        "not copied: the selection is past {} MiB",
                        MAX_COPY >> 20
                    ));
                    return;
                }
                None => None,
            },
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
                let Current { view, rendered, .. } = &mut self.current;
                match chord {
                    "Return" => view.step_hit(true, rendered, &mut self.metrics, rows),
                    "S-Return" => view.step_hit(false, rendered, &mut self.metrics, rows),
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

    fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64, extend: bool) {
        match phase {
            PointerPhase::Press => self.press(x, y, extend),
            PointerPhase::Move => self.drag(x, y),
            PointerPhase::Release => self.release(x, y),
        }
    }

    /// A press: in the address row, its caret; on the page, a click, a
    /// drag or a double or triple click begins, Shift extending the
    /// selection from its anchor.
    fn press(&mut self, x: i64, y: i64, extend: bool) {
        self.press = None;
        self.drag_scrolled = None;
        let click = self.click.take();
        let address = self.address_rect();
        if address.contains(x, y) {
            self.focus = Focus::Address;
            if let Some(entry) = TextEntry::new(self.surface, address) {
                self.address.place(entry, x, y, false);
            }
            return;
        }
        if !self.page_rect().contains(x, y) {
            return;
        }
        self.focus = Focus::Page;
        let now = self.clock.now();
        let near = (NEAR * self.scale()).unsigned_abs();
        let count = match click {
            Some(click)
                if !extend
                    && click.count < 3
                    && now.saturating_sub(click.released) <= MULTI_CLICK_MS
                    && click.at.0.abs_diff(x) <= near
                    && click.at.1.abs_diff(y) <= near =>
            {
                click.count + 1
            }
            _ => 1,
        };
        let link = self.link_at(x, y).filter(|_| count == 1 && !extend);
        let pane = self.pane();
        let Current { view, rendered, .. } = &mut self.current;
        let spot = view.spot(rendered, &pane, x, y);
        let caret = spot.map_or(view.top, |spot| spot.caret);
        let page = &rendered.page;
        let anchor = match view.selection {
            Some(selection) if extend => selection.anchor,
            _ => caret,
        };
        view.select(match count {
            _ if extend => Some(Selection {
                anchor,
                focus: caret,
            }),
            2 => spot
                .and_then(|spot| page.word(spot.under?))
                .map(|(anchor, focus)| Selection { anchor, focus }),
            3 => spot.map(|spot| Selection {
                anchor: Place::line(spot.caret.logical),
                focus: page.line_end(spot.caret.logical),
            }),
            _ => None,
        });
        self.press = Some(Press {
            from: (x, y),
            at: (x, y),
            anchor,
            link,
            count,
            selecting: extend,
            moved: false,
        });
    }

    /// The pointer moved with the button held: a press's drag past `NEAR`
    /// selects from it to the pointer.
    fn drag(&mut self, x: i64, y: i64) {
        let near = NEAR * self.scale();
        let Some(press) = &mut self.press else {
            return;
        };
        press.at = (x, y);
        press.moved |= (x, y) != press.from;
        if press.count == 1 && !press.selecting {
            let (from_x, from_y) = press.from;
            press.selecting = (x - from_x).abs() > near || (y - from_y).abs() > near;
        }
        if press.selecting {
            self.select_to(x, y);
        }
    }

    /// The selection from the press's anchor to where a point falls.
    fn select_to(&mut self, x: i64, y: i64) {
        let Some(press) = self.press else {
            return;
        };
        let pane = self.pane();
        let view = &mut self.current.view;
        if let Some(spot) = view.spot(&self.current.rendered, &pane, x, y) {
            view.select(Some(Selection {
                anchor: press.anchor,
                focus: spot.caret,
            }));
        }
    }

    /// The button's release: a click on a link follows it, and any other
    /// click is kept for the next press to pair with.
    fn release(&mut self, x: i64, y: i64) {
        self.drag_scrolled = None;
        let Some(press) = self.press.take() else {
            return;
        };
        if press.selecting {
            return;
        }
        match press.link {
            Some(link) => self.follow_link(link),
            None => {
                self.click = Some(Click {
                    released: self.clock.now(),
                    at: (x, y),
                    count: press.count,
                })
            }
        }
    }

    /// How many lines a selecting drag held above the page (less than
    /// none) or below its last line shown whole scrolls by at a time:
    /// none before the pointer has moved, and none on a page shorter than
    /// a line, which shows no line whole to select to.
    fn drag_past(&self) -> Option<i64> {
        let press = self.press.filter(|press| press.selecting && press.moved)?;
        let pane = self.pane();
        if pane.shown() == 0 {
            return None;
        }
        let height = i64::from(pane.line.height.max(1));
        let top = pane.rect.y;
        let bottom = top + pane.shown() as i64 * height;
        let y = press.at.1;
        let lines = if y < top {
            -(1 + (top - y) / height)
        } else if y >= bottom {
            1 + (y - bottom) / height
        } else {
            return None;
        };
        Some(lines.clamp(-DRAG_SCROLL_LINES, DRAG_SCROLL_LINES))
    }

    /// Scrolls a selecting drag held past the page's edge, at most once
    /// each `DRAG_SCROLL_MS`, and extends the selection to where the
    /// pointer falls on the lines then shown.
    fn drag_scroll(&mut self, now: u64) {
        let Some(lines) = self.drag_past() else {
            self.drag_scrolled = None;
            return;
        };
        if self
            .drag_scrolled
            .is_some_and(|at| now.saturating_sub(at) < DRAG_SCROLL_MS)
        {
            return;
        }
        self.drag_scrolled = Some(now);
        let before = (self.current.view.top, self.current.view.selection);
        self.scroll(isize::try_from(lines).unwrap_or(0));
        self.reselect();
        if (self.current.view.top, self.current.view.selection) != before {
            self.redraw = true;
        }
    }

    /// A selecting drag's selection extended again to where the pointer
    /// falls, after the lines under it moved.
    fn reselect(&mut self) {
        if let Some(press) = self.press.filter(|press| press.selecting) {
            self.select_to(press.at.0, press.at.1);
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
        let lines = rendered.page.lines.len();
        if let Some(bottom) = rendered.bottom(self.current.view.top, self.rows()) {
            parts.push(format!("{bottom} of {lines} lines"));
        }
        if let Some(note) = &doc.note {
            parts.push(note.clone());
        }
        if rendered.page.cut {
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
    (
        "/, Ctrl+F",
        "find and select a match; n and N next and previous",
    ),
    (
        "drag, double or triple click",
        "select text, a word or a line",
    ),
    ("Shift+click", "extend the selection"),
    (
        "Shift+arrows",
        "extend it a character or line; with Ctrl a word",
    ),
    ("Ctrl+A", "select the page"),
    (
        "Escape",
        "stop loading, else clear focus, find and selection",
    ),
];
const BROWSER_KEYS: &[(&str, &str)] = &[
    ("Ctrl+L, F6", "the address"),
    ("Alt+Left, Alt+Right", "back, forward"),
    ("Ctrl+R, F5", "reload"),
    (
        "Ctrl+C",
        "copy the selection, else the link's or page's address",
    ),
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
            Input::Pointer {
                phase,
                x,
                y,
                extend,
                ..
            } => {
                if phase == PointerPhase::Press {
                    self.message = None;
                }
                self.pointer(phase, x, y, extend);
            }
            Input::CancelPointer => {
                self.press = None;
                self.click = None;
                self.drag_scrolled = None;
            }
            Input::Wheel { rows, .. } => {
                self.message = None;
                self.scroll(rows);
                self.reselect();
            }
            Input::Resize(surface) => {
                self.surface = surface;
                self.current.fitted = PictureFit::Empty;
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

    fn poll(&mut self, now: u64) -> Flow {
        if self.loading.is_some() {
            self.settle();
            self.redraw = true;
        }
        self.drag_scroll(now);
        self.flow()
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        if self.drag_past().is_some() {
            DRAG_SCROLL_MS
        } else if self.loading.is_some() {
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
            self.current.fitted = PictureFit::Empty;
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
                let fitted = self.current.fitted.get_or_fit(|| {
                    let (width, height) =
                        picture.fit_size(page.width as usize, page.height as usize);
                    picture.fit(width, height)
                });
                if let Some(fitted) = fitted {
                    raster.image(page, page, fitted);
                } else {
                    picture_notice(surface, page, bounds, &mut sink);
                }
            }
            None if self.current.doc.picture_resources() => {
                picture_notice(surface, page, bounds, &mut sink);
            }
            None => {
                let rows = self.rows();
                let current = &mut self.current;
                current
                    .rendered
                    .settle(&mut self.metrics, current.view.top, rows);
                view::paint(
                    &self.current.rendered,
                    &self.current.view,
                    self.hover,
                    &self.pane(),
                    &mut sink,
                )
            }
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

    #[test]
    fn failed_picture_fit_is_remembered_until_reset() {
        let mut cache = PictureFit::Empty;
        let picture = td_ui::picture::Picture::new(1, 1, vec![255; 4]).unwrap();
        assert!(cache.get_or_fit(|| None).is_none());
        assert!(matches!(cache, PictureFit::Failed));
        let mut retried = false;
        assert!(cache
            .get_or_fit(|| {
                retried = true;
                picture.fit(1, 1)
            })
            .is_none());
        assert!(!retried);
        cache = PictureFit::Empty;
        assert!(cache.get_or_fit(|| picture.fit(1, 1)).is_some());
    }

    use crate::document::tests::Laid;
    use crate::document::Wrapped;
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

    /// The lines on the screen, as laid.
    fn on_screen(app: &App) -> Vec<Wrapped> {
        let top = app.current.view.top;
        app.current
            .rendered
            .shown(top, app.rows())
            .cloned()
            .collect()
    }

    /// Every line of the page, laid as the window lays them.
    fn every(app: &App) -> Laid {
        Laid::of(app.current.rendered.clone(), &mut app.metrics.clone())
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
        let text = shown(&on_screen(&app)[0].line);
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
        let all = every(&app);
        let line = all.link_lines()[0];
        let left = all.lines[line]
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
        app.mouse(PointerPhase::Press, (x, y), false);
        assert!(app.loading.is_none(), "followed at the release");
        app.mouse(PointerPhase::Release, (x, y), false);
        settle(&mut app);
        assert_eq!(app.title(), "A - td-web");
        assert_eq!(app.hover, None, "nothing under the pointer on the new page");
    }

    /// A link whose text is all past the pane's edge on a line that is
    /// not wrapped is laid nowhere, but takes focus on its line, nothing
    /// marked, the status row naming where it goes.
    #[test]
    fn a_link_past_an_unwrapped_lines_edge_takes_focus_and_is_named() {
        let page = format!(
            "<pre>{}<a href=\"/far\">far</a></pre><p><a href=\"/near\">near</a>",
            "x".repeat(300)
        );
        let mut app = app(site().page("https://example.org/w", "text/html", &page));
        typed(&mut app, "example.org/w");
        let pre = &on_screen(&app)[0];
        assert!(pre
            .line
            .pieces
            .iter()
            .all(|piece| piece.tag == document::NO_LINK));
        key(&mut app, "Tab");
        assert_eq!(app.current.view.focus, Some(0));
        assert_eq!(app.status_text(), "https://example.org/far");
        key(&mut app, "Tab");
        assert_eq!(app.status_text(), "https://example.org/near");
        // A pane wide enough lays it, and a click there follows it.
        let mut board = Board::default();
        app.input(Input::Resize(surface(3000)), &mut board);
        let pre = &on_screen(&app)[0];
        assert!(pre.line.pieces.iter().any(|piece| piece.tag == 0));
    }

    /// The status row says where the reader is by logical lines: the
    /// one the last line shown is part of, of the page's, so it needs no
    /// more of the page laid than is shown; and nothing on a page shown
    /// whole.
    #[test]
    fn the_status_row_counts_logical_lines() {
        let mut lines = String::new();
        for n in 0..200 {
            lines.push_str(&format!(
                "<p>line {n} with words enough to wrap twice here</p>"
            ));
        }
        let mut app = app(site().page("https://example.org/long", "text/html", &lines));
        let mut board = Board::default();
        app.input(Input::Resize(surface(200)), &mut board);
        typed(&mut app, "example.org/long");
        let count = app.current.rendered.page.lines.len();
        let shown = on_screen(&app);
        let bottom = shown.last().unwrap().logical + 1;
        assert!(bottom < shown.len(), "lines wrapped");
        assert_eq!(app.status_text(), format!("{bottom} of {count} lines"));
        key(&mut app, "End");
        assert_eq!(app.status_text(), format!("{count} of {count} lines"));
        typed(&mut app, "example.org/a");
        assert_eq!(app.status_text(), "");
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
        let family = |app: &App| on_screen(app)[0].line.pieces[0].style.family;
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

    /// The words the line starting at `top` shows.
    fn words(app: &App, top: Place) -> String {
        shown(&app.current.rendered.shown(top, 1).next().unwrap().line)
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
            assert!(top != Place::default(), "not the paragraph's start");
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
        assert!(top != Place::default());
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
        assert!(
            app.current.view.top > Place::default(),
            "find moved to a match"
        );
        let at = app.current.view.at;
        let hits = app.current.view.hits.len();
        key(&mut app, "Escape");
        key(&mut app, "Home");
        let mut board = Board::default();
        app.input(Input::Resize(surface(400)), &mut board);
        assert_eq!(
            app.current.view.top,
            Place::default(),
            "not moved to the match"
        );
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
        let laid = on_screen(&app);
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
        let logical = |app: &App| app.current.view.top.logical;
        assert_eq!(logical(&app), 41);
        let wide = every(&app).lines.len();
        let mut board = Board::default();
        app.input(Input::Resize(surface(200)), &mut board);
        let all = every(&app).lines;
        assert!(all.len() > wide);
        assert_eq!(logical(&app), 41);
        let top = app.current.view.top;
        let at = all.iter().position(|w| w.place() == top).unwrap();
        assert_eq!(all[at - 1].logical, 40, "its first line");
    }

    /// The pointer's presses, moves and releases, as methods so a point
    /// read from the app can be an argument.
    trait Pointing {
        fn mouse(&mut self, phase: PointerPhase, at: (i64, i64), extend: bool);
        /// A press at `from`, the pointer moved to `to` and released there.
        fn drag_from(&mut self, from: (i64, i64), to: (i64, i64));
        /// A click at `at`, `ms` after the window began.
        fn click_at(&mut self, at: (i64, i64), ms: u64);
        fn shift_click(&mut self, at: (i64, i64));
    }

    impl Pointing for App {
        fn mouse(&mut self, phase: PointerPhase, (x, y): (i64, i64), extend: bool) {
            let mut board = Board::default();
            self.input(
                Input::Pointer {
                    phase,
                    x,
                    y,
                    extend,
                    follow: false,
                },
                &mut board,
            );
        }

        fn drag_from(&mut self, from: (i64, i64), to: (i64, i64)) {
            self.mouse(PointerPhase::Press, from, false);
            self.mouse(PointerPhase::Move, to, false);
            self.mouse(PointerPhase::Release, to, false);
        }

        fn click_at(&mut self, at: (i64, i64), ms: u64) {
            self.clock.fixed = Some(ms);
            self.mouse(PointerPhase::Press, at, false);
            self.mouse(PointerPhase::Release, at, false);
        }

        fn shift_click(&mut self, at: (i64, i64)) {
            self.mouse(PointerPhase::Press, at, true);
            self.mouse(PointerPhase::Release, at, false);
        }
    }

    /// What Ctrl+C copies.
    fn copied(app: &mut App) -> Vec<String> {
        let mut board = Board::default();
        app.input(
            Input::Key {
                chord: "C-c",
                repeat: false,
            },
            &mut board,
        );
        board.copies
    }

    /// A point a pixel into glyph `glyph` of the `row`th line shown: its
    /// left half on the cell.
    fn point(app: &App, row: usize, glyph: usize) -> (i64, i64) {
        let x = on_screen(app)[row].line.glyphs[glyph].x;
        let page = app.page_rect();
        let height = i64::from(app.metrics.line().height);
        (
            page.x + app.inset() + i64::from(x) + 1,
            page.y + row as i64 * height + 1,
        )
    }

    /// Two paragraphs on a pane 200 pixels wide, the first wrapped.
    fn two_paragraphs() -> App {
        let page = "<p>alpha beta gamma delta epsilon zeta</p><p>second para</p>";
        let mut app = app(site().page("https://example.org/t", "text/html", page));
        let mut board = Board::default();
        app.input(Input::Resize(surface(200)), &mut board);
        typed(&mut app, "example.org/t");
        let lines: Vec<String> = on_screen(&app).iter().map(|w| shown(&w.line)).collect();
        assert_eq!(
            lines,
            ["alpha beta gamma delta", "epsilon zeta", "", "second para"]
        );
        app
    }

    #[test]
    fn a_drag_selects_across_wrapped_and_logical_lines_and_copies_them() {
        let mut app = two_paragraphs();
        app.drag_from(point(&app, 0, 6), point(&app, 3, 7));
        assert_eq!(app.current.view.selection.unwrap().anchor.logical, 0);
        // The space the wrap dropped is the page's, and so is the blank
        // line between the paragraphs.
        assert_eq!(
            copied(&mut app),
            ["beta gamma delta epsilon zeta\n\nsecond "]
        );
        assert_eq!(app.status_text(), "copied");
        // Dragged up past where it began, it selects before it.
        app.drag_from(point(&app, 1, 8), point(&app, 0, 11));
        assert_eq!(copied(&mut app), ["gamma delta epsilon "]);
        // A plain click clears it.
        app.click_at(point(&app, 3, 1), 0);
        assert_eq!(app.current.view.selection, None);
    }

    #[test]
    fn a_double_click_selects_a_word_and_a_triple_click_its_logical_line() {
        let mut app = two_paragraphs();
        let gamma = point(&app, 0, 12);
        app.click_at(gamma, 1000);
        assert_eq!(app.current.view.selection, None, "a click selects nothing");
        app.click_at(gamma, 1000 + MULTI_CLICK_MS);
        assert_eq!(copied(&mut app), ["gamma"]);
        app.click_at((gamma.0 + NEAR, gamma.1 - 1), 1600);
        assert_eq!(
            copied(&mut app),
            ["alpha beta gamma delta epsilon zeta"],
            "from its first wrapped line to its last"
        );
        // A fourth is a first again, as is a slow or distant second.
        app.click_at(gamma, 1700);
        assert_eq!(app.current.view.selection, None);
        app.click_at(gamma, 1701 + MULTI_CLICK_MS);
        assert_eq!(app.current.view.selection, None, "slow");
        app.click_at((gamma.0 + NEAR + 1, gamma.1), 2400);
        assert_eq!(app.current.view.selection, None, "distant");
        // On a continuation, the word and its logical line; a drag after
        // a double click does not move what it selected.
        let epsilon = point(&app, 1, 2);
        app.click_at(epsilon, 5000);
        app.clock.fixed = Some(5100);
        app.mouse(PointerPhase::Press, epsilon, false);
        app.mouse(PointerPhase::Move, point(&app, 3, 3), false);
        app.mouse(PointerPhase::Release, point(&app, 3, 3), false);
        assert_eq!(copied(&mut app), ["epsilon"]);
        // Off a word, a double click selects nothing.
        let space = point(&app, 0, 5);
        app.click_at(space, 9000);
        app.click_at(space, 9100);
        assert_eq!(app.current.view.selection, None);
    }

    #[test]
    fn a_shift_click_extends_the_selection_from_its_anchor() {
        let mut app = two_paragraphs();
        app.drag_from(point(&app, 0, 6), point(&app, 0, 10));
        assert_eq!(copied(&mut app), ["beta"]);
        app.shift_click(point(&app, 0, 17));
        assert_eq!(copied(&mut app), ["beta gamma "]);
        app.shift_click(point(&app, 0, 0));
        assert_eq!(copied(&mut app), ["alpha "], "the anchor kept");
        // With nothing selected it starts at the press, and its drag
        // extends at once.
        key(&mut app, "Escape");
        app.mouse(PointerPhase::Press, point(&app, 3, 0), true);
        app.mouse(PointerPhase::Move, point(&app, 3, 1), false);
        app.mouse(PointerPhase::Release, point(&app, 3, 1), false);
        assert_eq!(copied(&mut app), ["s"]);
    }

    /// A press and release within `NEAR` of each other on a link follow
    /// it; a drag further selects and follows nothing, as does a Shift
    /// press or a press the pointer's leaving cancels.
    #[test]
    fn a_click_on_a_link_follows_it_and_a_drag_from_it_selects() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        let link = point(&app, 0, 1);
        let near = (link.0 + NEAR, link.1 + NEAR);
        app.mouse(PointerPhase::Press, link, false);
        app.mouse(PointerPhase::Move, near, false);
        app.mouse(PointerPhase::Release, near, false);
        settle(&mut app);
        assert_eq!(app.title(), "A - td-web");
        key(&mut app, "M-Left");
        settle(&mut app);
        let far = (link.0 + NEAR + 1, link.1);
        app.mouse(PointerPhase::Press, link, false);
        app.mouse(PointerPhase::Move, far, false);
        app.mouse(PointerPhase::Release, far, false);
        assert!(app.loading.is_none(), "not followed");
        assert_eq!(copied(&mut app), ["o"]);
        app.shift_click(point(&app, 0, 0));
        assert!(app.loading.is_none(), "not followed");
        assert_eq!(copied(&mut app), ["t"]);
        app.mouse(PointerPhase::Press, link, false);
        let mut board = Board::default();
        app.input(Input::CancelPointer, &mut board);
        app.mouse(PointerPhase::Release, link, false);
        assert!(app.loading.is_none(), "cancelled");
    }

    #[test]
    fn a_drag_past_the_pane_scrolls_at_a_bounded_rate_and_extends() {
        let mut lines = String::new();
        for n in 0..200 {
            lines.push_str(&format!("<p>line {n}</p>"));
        }
        let mut app = app(site().page("https://example.org/long", "text/html", &lines));
        typed(&mut app, "example.org/long");
        let start = point(&app, 0, 0);
        let pane = app.pane();
        let height = i64::from(pane.line.height);
        let shown = pane.shown();
        let bottom = pane.rect.y + shown as i64 * height;
        assert_eq!(app.wait_ms(0), IDLE_WAIT_MS);
        app.mouse(PointerPhase::Press, start, false);
        app.mouse(PointerPhase::Move, (start.0, bottom), false);
        assert_eq!(app.wait_ms(0), DRAG_SCROLL_MS);
        let top = |app: &App| app.current.view.top.logical;
        let focus = |app: &App| app.current.view.selection.unwrap().focus;
        assert_eq!(top(&app), 0);
        assert_eq!(focus(&app).logical, shown - 1, "the last line's end");
        app.poll(1000);
        assert_eq!(top(&app), 1);
        app.poll(1000 + DRAG_SCROLL_MS - 1);
        assert_eq!(top(&app), 1, "not sooner");
        app.poll(1000 + DRAG_SCROLL_MS);
        assert_eq!(top(&app), 2);
        assert_eq!(focus(&app).logical, 2 + shown - 1);
        // Further past the edge, faster, but at most DRAG_SCROLL_LINES.
        app.mouse(PointerPhase::Move, (start.0, bottom + 2 * height), false);
        app.poll(2000);
        assert_eq!(top(&app), 5);
        app.mouse(PointerPhase::Move, (start.0, bottom + 50 * height), false);
        app.poll(3000);
        assert_eq!(top(&app), 5 + DRAG_SCROLL_LINES as usize);
        assert_eq!(app.current.view.selection.unwrap().anchor, Place::default());
        // Above the pane it scrolls back, to the first line's start.
        app.mouse(PointerPhase::Move, (start.0, pane.rect.y - 1), false);
        app.poll(4000);
        assert_eq!(top(&app), 8);
        assert_eq!(focus(&app), Place::line(8));
        // Inside the pane, or released, it stops.
        app.mouse(PointerPhase::Move, (start.0, pane.rect.y + 1), false);
        app.poll(5000);
        assert_eq!(top(&app), 8);
        app.mouse(PointerPhase::Move, (start.0, bottom), false);
        app.mouse(PointerPhase::Release, (start.0, bottom), false);
        assert_eq!(app.wait_ms(0), IDLE_WAIT_MS);
        app.poll(6000);
        assert_eq!(top(&app), 8);
        let copy = copied(&mut app);
        assert!(copy[0].starts_with("line 0\n\nline 1\n"), "{copy:?}");
    }

    /// A Shift press on the line partly shown below the last whole one
    /// extends the selection there and scrolls nothing until the pointer
    /// moves; a drag held past the page's end redraws only when it moves
    /// something; and the wheel turned during a drag moves the selection's
    /// end to where the pointer now falls.
    #[test]
    fn only_a_drag_scrolls_and_only_a_change_redraws() {
        let mut lines = String::new();
        for n in 0..200 {
            lines.push_str(&format!("<p>line {n}</p>"));
        }
        let mut app = app(site().page("https://example.org/long", "text/html", &lines));
        typed(&mut app, "example.org/long");
        let pane = app.pane();
        let height = i64::from(pane.line.height);
        let bottom = pane.rect.y + pane.shown() as i64 * height;
        assert!(
            bottom + 1 < pane.rect.y + i64::from(pane.rect.height),
            "a part line"
        );
        app.mouse(PointerPhase::Press, (40, bottom + 1), true);
        app.poll(1000);
        app.mouse(PointerPhase::Move, (40, bottom + 1), false);
        app.poll(2000);
        app.mouse(PointerPhase::Release, (40, bottom + 1), false);
        assert_eq!(app.current.view.top, Place::default(), "not scrolled");
        let focus = app.current.view.selection.unwrap().focus;
        assert_eq!(focus.logical, pane.shown() - 1, "the last whole line's end");
        // At the page's end a drag below it scrolls nothing and asks for
        // no paint.
        key(&mut app, "End");
        let top = app.current.view.top;
        app.mouse(PointerPhase::Press, (40, pane.rect.y + 1), false);
        app.mouse(PointerPhase::Move, (40, bottom), false);
        app.poll(3000);
        app.redraw = false;
        app.poll(3100);
        assert!(!app.redraw);
        assert_eq!(app.current.view.top, top);
        // The wheel turned while the pointer is held over the page.
        app.mouse(PointerPhase::Move, (40, pane.rect.y + 1), false);
        let first = app.current.view.selection.unwrap().focus;
        let mut board = Board::default();
        app.input(
            Input::Wheel {
                rows: -3,
                columns: 0,
            },
            &mut board,
        );
        let now = app.current.view.selection.unwrap().focus;
        assert_eq!(now.logical, app.current.view.top.logical);
        assert!(now < first, "{now:?} {first:?}");
        app.mouse(PointerPhase::Release, (40, pane.rect.y + 1), false);
    }

    /// A pane shorter than a line shows no line whole: a press there
    /// anchors at the top and a drag does not scroll.
    #[test]
    fn a_pane_shorter_than_a_line_does_not_drag_scroll() {
        let mut lines = String::new();
        for n in 0..50 {
            lines.push_str(&format!("<p>line {n}</p>"));
        }
        let mut app = app(site().page("https://example.org/long", "text/html", &lines));
        typed(&mut app, "example.org/long");
        key(&mut app, "PageDown");
        let height = 2 * td_ui::chrome::ROW + 8;
        let mut board = Board::default();
        app.input(
            Input::Resize(Surface::new(800, height, Scale::new(1).unwrap()).unwrap()),
            &mut board,
        );
        let pane = app.pane();
        assert_eq!(pane.shown(), 0);
        let top = app.current.view.top;
        assert!(top > Place::default());
        app.mouse(PointerPhase::Press, (40, pane.rect.y + 1), true);
        app.mouse(PointerPhase::Move, (40, pane.rect.y + 40), false);
        assert_eq!(app.wait_ms(0), IDLE_WAIT_MS);
        app.poll(1000);
        assert_eq!(app.current.view.top, top);
        app.mouse(PointerPhase::Release, (40, pane.rect.y + 40), false);
        assert_eq!(app.current.view.selection.unwrap().anchor, top);
    }

    /// The glyphs the page paints on the selection's ground, spaces left
    /// out.
    fn painted_selection(app: &App) -> String {
        let mut marked = String::new();
        view::paint(
            &app.current.rendered,
            &app.current.view,
            None,
            &app.pane(),
            &mut |draw| {
                if let td_ui::raster::Primitive::Glyph { scalar, style, .. } = draw.primitive {
                    if style.background == td_ui::raster::SELECTED && scalar != ' ' {
                        marked.push(scalar);
                    }
                }
            },
        );
        marked
    }

    #[test]
    fn a_selection_survives_scrolling_past_the_wrap_window_a_resize_and_f8() {
        // One logical line of some 500 wrapped lines, read 30 lines in.
        let words: Vec<String> = (0..10_000).map(|n| format!("w{n}")).collect();
        let page = format!("<div>{}</div>", words.join(" "));
        let mut app = app(site().page("https://example.org/p", "text/html", &page));
        typed(&mut app, "example.org/p");
        for _ in 0..30 {
            key(&mut app, "Down");
        }
        app.drag_from(point(&app, 0, 0), point(&app, 2, 3));
        let selection = app.current.view.selection;
        let text = copied(&mut app);
        assert!(text[0].len() > 100, "{text:?}");
        let words: String = text[0].split(' ').collect();
        assert_eq!(painted_selection(&app), words);
        key(&mut app, "End");
        let first = crate::document::tests::laid(&mut app.current.rendered)
            .front()
            .unwrap()
            .place();
        assert!(first > selection.unwrap().focus, "its lines are not laid");
        assert_eq!(painted_selection(&app), "");
        let mut board = Board::default();
        for change in [Some(surface(300)), None, Some(surface(800)), None] {
            match change {
                Some(surface) => {
                    app.input(Input::Resize(surface), &mut board);
                }
                None => key(&mut app, "F8"),
            }
            assert_eq!(app.current.view.selection, selection);
            assert_eq!(copied(&mut app), text);
        }
        key(&mut app, "Home");
        for _ in 0..30 {
            key(&mut app, "Down");
        }
        assert_eq!(painted_selection(&app), words);
    }

    #[test]
    fn ctrl_c_copies_a_rows_then_the_pages_selection_then_the_link_then_the_address() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        key(&mut app, "Tab");
        assert_eq!(copied(&mut app), ["https://example.org/a"]);
        app.drag_from(point(&app, 0, 0), point(&app, 0, 2));
        assert_eq!(app.current.view.focus, None, "the selection replaces it");
        assert_eq!(copied(&mut app), ["to"]);
        key(&mut app, "C-l");
        assert_eq!(copied(&mut app), ["https://example.org/"], "the row's");
        key(&mut app, "Escape");
        assert_eq!(copied(&mut app), ["to"]);
        key(&mut app, "Tab");
        assert_eq!(app.current.view.selection, None, "the link replaces it");
        assert_eq!(copied(&mut app), ["https://example.org/a"]);
        key(&mut app, "C-a");
        assert_eq!(copied(&mut app), ["to a mail"]);
        key(&mut app, "Escape");
        assert_eq!(app.current.view.selection, None);
        assert_eq!(copied(&mut app), ["https://example.org/"], "the page's");
    }

    #[test]
    fn navigation_clears_the_selection_and_a_press_held_over_it() {
        let mut app = two_paragraphs();
        key(&mut app, "C-a");
        assert!(app.current.view.selection.is_some());
        typed(&mut app, "example.org/a");
        assert_eq!(app.current.view.selection, None);
        key(&mut app, "M-Left");
        settle(&mut app);
        assert_eq!(app.title(), "https://example.org/t - td-web");
        assert_eq!(app.current.view.selection, None, "not kept in the history");
        // A page arriving under a held press is not dragged over.
        app.mouse(PointerPhase::Press, point(&app, 0, 0), false);
        key(&mut app, "C-r");
        settle(&mut app);
        app.mouse(PointerPhase::Move, point(&app, 3, 3), false);
        app.mouse(PointerPhase::Release, point(&app, 3, 3), false);
        assert_eq!(app.current.view.selection, None);
        assert!(app.loading.is_none());
    }

    #[test]
    fn a_selection_past_the_copy_bound_is_not_copied() {
        let line = "x".repeat(99);
        let lines = MAX_COPY / 100 + 1;
        let body = format!("{line}\n").repeat(lines);
        let mut app = app(site().page("https://example.org/big", "text/plain", &body));
        typed(&mut app, "example.org/big");
        key(&mut app, "C-a");
        assert_eq!(copied(&mut app), Vec::<String>::new());
        assert_eq!(
            app.status_text(),
            format!("not copied: the selection is past {} MiB", MAX_COPY >> 20)
        );
        // Up to the last line's start, it is within the bound.
        key(&mut app, "S-Up");
        let copy = copied(&mut app);
        assert_eq!(copy[0].len(), (lines - 1) * 100);
        assert!(copy[0].len() <= MAX_COPY);
        assert_eq!(copy[0], body[..(lines - 1) * 100]);
    }

    #[test]
    fn find_selects_its_match_and_shift_keys_extend_it() {
        let page = "<p>alpha beta gamma</p><p>delta</p>";
        let mut app = app(site().page("https://example.org/f", "text/html", page));
        typed(&mut app, "example.org/f");
        key(&mut app, "/");
        for c in "beta".chars() {
            key(&mut app, &c.to_string());
        }
        assert_eq!(copied(&mut app), ["beta"], "from the find row");
        key(&mut app, "Escape");
        for (chord, copy) in [
            ("S-Right", "beta "),
            ("C-S-Right", "beta gamma"),
            ("S-Down", "beta gamma\n"),
            ("S-Down", "beta gamma\n\ndelta"),
            ("S-Up", "beta gamma\n\n"),
            ("S-Left", "beta gamma\n"),
            ("C-S-Left", "beta gamma"),
            ("C-S-Left", "beta "),
        ] {
            key(&mut app, chord);
            assert_eq!(copied(&mut app), [copy], "{chord}");
        }
        // With nothing selected, from the first line shown, or from the
        // focused link.
        key(&mut app, "Escape");
        key(&mut app, "S-Right");
        assert_eq!(copied(&mut app), ["a"]);
        let mut app = app_over(Arc::new(site()));
        typed(&mut app, "example.org");
        key(&mut app, "Tab");
        key(&mut app, "Tab");
        key(&mut app, "C-S-Right");
        assert_eq!(copied(&mut app), ["mail"]);
    }

    /// A line that is not wrapped copies its kept text whole, past what
    /// the pane shows of it.
    #[test]
    fn a_line_not_wrapped_copies_its_kept_text_past_the_panes_edge() {
        let page = format!("<pre>{}END</pre><p>after</p>", "x".repeat(300));
        let mut app = app(site().page("https://example.org/pre", "text/html", &page));
        typed(&mut app, "example.org/pre");
        let laid = on_screen(&app)[0].line.glyphs.len();
        assert!(laid < 300);
        let at = point(&app, 0, 5);
        for ms in [1000, 1100, 1200] {
            app.click_at(at, ms);
        }
        let whole = format!("{}END", "x".repeat(300));
        assert_eq!(copied(&mut app), std::slice::from_ref(&whole));
        // A drag into the inset right of the lines takes it whole too.
        let pane = app.pane();
        let right = pane.rect.x + i64::from(pane.rect.width) - 1;
        app.drag_from(point(&app, 0, 0), (right, at.1));
        assert_eq!(copied(&mut app), [whole]);
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
        let rendered = on_screen(&app);
        let first = &rendered[0].line;
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
        // A drag selects by the glyphs' pixels: from the left half of the
        // first 'm' to the left half of the space after the last; and a
        // double click on an 'i' selects its word.
        let pen = |at: usize| left + i64::from(first.glyphs[at].x) + 1;
        app.drag_from((pen(5), page.y + 1), (pen(9), page.y + 1));
        assert_eq!(copied(&mut app), ["mmmm"]);
        app.click_at((pen(2), page.y + 1), 1000);
        app.click_at((pen(2), page.y + 1), 1100);
        assert_eq!(copied(&mut app), ["iiii"]);
        // A match spans its glyphs' pixels.
        key(&mut app, "/");
        for c in "after".chars() {
            key(&mut app, &c.to_string());
        }
        assert_eq!(app.current.view.hits.len(), 1);
        let mut marked = Vec::new();
        view::paint(
            &app.current.rendered,
            &app.current.view,
            None,
            &app.pane(),
            &mut |draw| {
                if let td_ui::raster::Primitive::Glyph {
                    x, scalar, style, ..
                } = draw.primitive
                {
                    if style.background == td_ui::raster::SELECTED {
                        marked.push((x - left, scalar));
                    }
                }
            },
        );
        let after: String = marked.iter().map(|&(_, scalar)| scalar).collect();
        assert_eq!(after, "after");
        assert_eq!(marked[0].0, i64::from(first.glyphs[17].x));
        key(&mut app, "Escape");
        // A preformatted line is in the monospace face, one advance a
        // glyph, and not wrapped.
        let pre = &rendered[2].line;
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
        let serif = &on_screen(&app)[0].line;
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
