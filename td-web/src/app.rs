//! The browser in its window (DESIGN.md §8): the address row, the page
//! and the status row; the history; and each navigation's fetch on a
//! thread of its own, which hands the reply to the one render thread to
//! be parsed, styled and laid out (DESIGN.md §10), and waits for it, its
//! document taken when the window next polls. The window paints, finds,
//! selects and copies from the laid page; a new width, scale or face asks
//! the render thread to lay it out again. A save into Downloads runs on a
//! worker's thread too.

use std::borrow::Cow;
use std::fmt;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use td_alloc::BudgetShared;
use td_html::document::NodeId;
use td_ui::chrome::{Status, TextEntry, ROW};
use td_ui::clipboard::{OwnedText, TextError};
use td_ui::entry_model::{Action, EntryModel};
use td_ui::keys::Section;
use td_ui::picture::Fitted;
use td_ui::prose::{Faces, Metrics};
use td_ui::raster::{
    fill, Draw, Primitive, Raster, Rect, Scale, Surface, TextFamily, CHROME, PAPER,
};
use td_ui::window::{Clipboard, Flow, Handler, Input, PointerPhase};
use td_ui::CELL_WIDTH;
use td_url::{Reference, Url};

use crate::document::{self, Answer, Body, Document, Dropped, Lane, Made, Renderer};
use crate::fetch;
use crate::fonts::Measure;
use crate::page::{Laid, Want};
use crate::paint::{self, Marks, Scratch};
use crate::pictures::{self, Pictures};
use crate::pipeline::{self, Again, Refusal, Rung};
use crate::save;
use crate::subresource::{self, Slots};
use crate::text::{self, Move, Place};
use crate::tree::Source;
use crate::view::{self, Anchor, Pane, Selection, View};

const MAX_ADDRESS: usize = 4096;
const MAX_QUERY: usize = crate::search::MAX_QUERY;
/// The most pages each of back and forward keeps.
pub const MAX_HISTORY: usize = 100;
/// The most fetch threads at once, navigations and saves together.
pub const MAX_WORKERS: usize = 4;
/// Cells a `Left` or `Right` press, or a wheel's column, scrolls across.
const ACROSS: i64 = 4;
const LOADING_WAIT_MS: u64 = 50;
const IDLE_WAIT_MS: u64 = 1000;
/// The least viewport, in ems of td-ui's prose size: a narrower pane
/// shows the page's left and scrolls across it.
pub const MIN_WRAP_EMS: i64 = 8;
/// The most bytes a copy takes.
pub const MAX_COPY: usize = td_ui::clipboard::MAX_BYTES;
/// How near, in pixels at scale one, a press counts as a click's.
const NEAR: i64 = 4;
const MULTI_CLICK_MS: u64 = td_ui::messages::MULTI_CLICK_MS;
const DRAG_SCROLL_MS: u64 = 50;
const DRAG_SCROLL_LINES: i64 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Address,
    Page,
    Find,
}

/// A history entry: the page's URL, its fragment and the top's place.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    url: Url,
    fragment: Option<String>,
    top: Option<Anchor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Motion {
    New,
    Back,
    Forward,
    Reload,
}

type Arrival = (Document, Option<String>);

const FETCHING: u8 = 0;
const WRITING: u8 = 1;
const STOPPED: u8 = 2;

struct Saving {
    answer: Receiver<Option<String>>,
    state: Arc<AtomicU8>,
    thread: JoinHandle<()>,
}

struct Loading {
    url: Url,
    fragment: Option<String>,
    motion: Motion,
    top: Option<Anchor>,
    /// Taken to be let go on the render thread when it is stopped.
    answer: Option<Receiver<Option<Arrival>>>,
    stopped: Stopped,
    /// The fetch's ask for room for a picture, and the window's answer:
    /// whether it let the page shown go (DESIGN.md §10, "Memory").
    room: Receiver<()>,
    released: Sender<bool>,
    /// Room given for a picture, owed to the fetch once what was let go
    /// has left the window: whether the page shown was let go.
    owed: Option<bool>,
    /// A reload of the page let go for a picture's room: stopped or
    /// overtaken, its entry is held again.
    restores: bool,
    /// Told once the page waits for its sheets.
    styles: Receiver<()>,
    styling: bool,
}

/// A navigation's stop flag, raised when its `Loading` goes.
struct Stopped(Arc<AtomicBool>);

/// A navigation's channels to the window, on its fetch thread.
struct Told {
    send: Sender<Option<Arrival>>,
    room: Sender<()>,
    released: Receiver<bool>,
    styles: Sender<()>,
}

/// What a navigation's fetch thread fetches its page's sheets with.
struct Sheets {
    fetcher: fetch::Shared,
    slots: Slots,
    wait: Duration,
}

impl Drop for Stopped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A relayout the render thread is making.
struct Relayout {
    answer: Answer,
    cancel: document::Cancel,
    /// The top's place when it was asked.
    top: Option<Anchor>,
    /// The focused link's element when it was asked.
    focused: Option<NodeId>,
    /// What it lays the page out for.
    want: Want,
    /// The old layout was let go first.
    let_go: bool,
}

impl Drop for Relayout {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Debug)]
struct Press {
    from: (i64, i64),
    at: (i64, i64),
    anchor: Place,
    link: Option<usize>,
    /// An image with no link under the press, which a click opens.
    image: Option<td_html::document::NodeId>,
    count: u8,
    selecting: bool,
    moved: bool,
}

#[derive(Clone, Copy, Debug)]
struct Click {
    released: u64,
    at: (i64, i64),
    count: u8,
}

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

struct Current {
    doc: Document,
    fragment: Option<String>,
    view: View,
    /// The measure the shown layout was laid with: its faces, scale and
    /// body family, so replayed advances agree with it.
    measure: Option<Measure>,
    relayout: Option<Relayout>,
    /// A relayout wanted while one runs: the newest only.
    again: bool,
    /// The page's own styles are shown; `F9` clears it for this page.
    styles: bool,
    /// The last relayout was refused and the old layout kept.
    kept: bool,
    find_refused: bool,
    /// The last paint was cut by one of its bounds.
    cut: bool,
    fitted: PictureFit,
    /// Paper left where a page was let go for a picture's room: the
    /// entry of the page it was, for history, a reload and a stop.
    held: Option<Entry>,
    /// The page's inline pictures (DESIGN.md §10, "Subresources").
    pictures: Option<Pictures>,
    /// A navigation cancelled the relayout that ran beside the layout
    /// shown: laid again once no navigation is out.
    relay_after: bool,
    /// The want last laid again for copies refused beside the old
    /// layout's: once a want, so a shortage never loops.
    refit: Option<Want>,
    /// The next relayout is asked though it wants what the layout shown
    /// was laid for: the old layout's copies, let go ahead of it on the
    /// lane, leave room for those refused.
    refitting: bool,
}

impl Current {
    fn new(doc: Document, fragment: Option<String>) -> Self {
        Self {
            doc,
            fragment,
            view: View::default(),
            measure: None,
            relayout: None,
            again: false,
            styles: true,
            kept: false,
            find_refused: false,
            cut: false,
            fitted: PictureFit::Empty,
            held: None,
            pictures: None,
            relay_after: false,
            refit: None,
            refitting: false,
        }
    }

    fn laid(&self) -> Option<&BudgetShared<Laid>> {
        self.doc.page()?.laid.as_ref()
    }
}

#[derive(Default)]
enum PictureFit {
    #[default]
    Empty,
    Ready(crate::memory::Resident<Fitted>),
    Failed,
}
impl PictureFit {
    fn get_or_fit(
        &mut self,
        fit: impl FnOnce() -> Option<crate::memory::Resident<Fitted>>,
    ) -> Option<&Fitted> {
        if matches!(self, Self::Empty) {
            *self = fit().map_or(Self::Failed, Self::Ready);
        }
        match self {
            Self::Ready(image) => Some(image),
            _ => None,
        }
    }
}

/// The measure `doc`'s layout was laid with, made anew in `slot` when
/// the layout's scale or face changed, and a handle on the layout.
fn measure_of<'m>(
    doc: &Document,
    slot: &'m mut Option<Measure>,
    renderer: &Renderer,
) -> Option<(BudgetShared<Laid>, &'m mut Measure)> {
    let laid = doc.page()?.laid.as_ref()?.try_clone().ok()?;
    let want = laid.want;
    let fits = slot
        .as_ref()
        .is_some_and(|m| m.scale().value() == usize::from(want.scale) && m.body() == want.body);
    if !fits {
        let scale = Scale::new(want.scale).unwrap_or_default();
        let metrics = Metrics::new_in(renderer.faces(), scale, renderer.budget());
        *slot = Some(Measure::new(metrics, want.body));
    }
    let measure = slot.as_mut()?;
    Some((laid, measure))
}

/// The image `item` is, when it is in no link: one a click opens.
fn unlinked_image(laid: &Laid, item: Option<text::Item>) -> Option<NodeId> {
    let item = item?;
    let text::Piece::Replaced(i) = item.piece else {
        return None;
    };
    let node = laid
        .lines()
        .get(text::line_of(laid, item.start))?
        .replaced
        .get(i)?
        .node;
    laid.tree().link(node).is_none().then_some(node)
}

/// The start of the scalar under window point (x, y), by its extent:
/// where a double click takes its word. None off text.
fn scalar_under(
    laid: &Laid,
    measure: &mut Measure,
    view: &View,
    pane: &Pane,
    x: i64,
    y: i64,
) -> Option<Place> {
    let (_, item) = view.spot(laid, measure, pane, x, y)?;
    let item = item?;
    let text::Piece::Text(at) = item.piece else {
        return None;
    };
    let line = laid.lines().get(text::line_of(laid, item.start))?;
    let fragment = line.fragments.get(at)?;
    let (px, _) = view.point(pane, x, y);
    let mut found = None;
    text::extents(laid, measure, line, fragment, |e| {
        if found.is_none() && e.x <= px && px < e.x + e.width {
            found = Some(e.start);
        }
    });
    found
}

/// Sets the selection; selecting lets the focused link go (DESIGN.md §8,
/// "One of a selection and a focused link").
fn select(view: &mut View, selection: Option<Selection>) {
    if selection.is_some() {
        view.focus = None;
    }
    view.selection = selection;
}

/// Steps find's current match, which becomes the selection.
fn step_hit(view: &mut View, laid: &Laid, measure: &mut Measure, pane: &Pane, forward: bool) {
    view.step_hit(laid, measure, pane, forward);
    if !view.hits().is_empty() {
        view.focus = None;
    }
}

/// The static notice a page or picture td-web could not keep shows: rung
/// 4, painted from static text.
fn notice(surface: Surface, page: Rect, bounds: Rect, sink: &mut dyn FnMut(td_ui::raster::Draw)) {
    fill(page, PAPER, bounds, sink);
    td_ui::raster::text_run(
        surface.scale,
        "Page unavailable: memory or decode budget exhausted.".chars(),
        (page.x, page.y),
        page,
        td_ui::raster::GlyphStyle::regular(td_ui::raster::INK, PAPER),
        bounds,
        sink,
    );
}

pub struct App {
    fetcher: fetch::Shared,
    renderer: Renderer,
    workers: Arc<AtomicUsize>,
    /// The subresource fetches out, every page's.
    slots: Slots,
    /// How long a page waits for its sheets.
    style_wait: Duration,
    /// The bytes of a page's pictures term.
    pictures_term: usize,
    /// The overlap steps one layout's fitted copies take (`FIT_WORK`).
    fit_work: u64,
    /// How long a picture's fetch is given.
    picture_wait: Duration,
    /// The most subresource slots its pictures take at once.
    picture_slots: usize,
    surface: Surface,
    focus: Focus,
    address: EntryModel,
    query: EntryModel,
    current: Current,
    back: Vec<Entry>,
    forward: Vec<Entry>,
    loading: Option<Loading>,
    message: Option<Cow<'static, str>>,
    saving: Option<Saving>,
    home: Option<PathBuf>,
    pointer: Option<(i64, i64)>,
    hover: Option<usize>,
    /// An image in no link under the pointer, which a click opens.
    hover_image: Option<NodeId>,
    title: String,
    redraw: bool,
    quit: bool,
    show_keys: bool,
    /// The window's own prose metrics, for its body line.
    metrics: Metrics,
    serif: bool,
    press: Option<Press>,
    click: Option<Click>,
    clock: Clock,
    drag_scrolled: Option<u64>,
    scratch: Scratch,
}

impl Drop for App {
    /// The job the render thread runs is cancelled before the renderer
    /// joins it, so closing does not wait for a page no one will see.
    fn drop(&mut self) {
        self.end_loading();
        if let Some(relayout) = &self.current.relayout {
            relayout.cancel.store(true, Ordering::SeqCst);
        }
        if let Some(pictures) = &self.current.pictures {
            pictures.leave();
        }
    }
}

struct Worker(Arc<AtomicUsize>);

impl Drop for Worker {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A navigation's fetch and its document, on its fetch thread. A
/// picture the root has no room for asks the window for room and, given
/// it, is tried once more. A page whose sheets are to be fetched waits
/// for them here (DESIGN.md §10, "Waiting").
fn arrive(
    sheets: &Sheets,
    renderer: &Renderer,
    url: Url,
    fragment: Option<String>,
    want: Want,
    stopped: &Arc<AtomicBool>,
    told: &Told,
) -> Option<Arrival> {
    match fetch::navigate(sheets.fetcher.as_ref(), url.clone()) {
        Ok(mut loaded) => {
            let came = Instant::now();
            let content_type = loaded.reply.header("content-type").map(str::to_string);
            // Moved out, not copied: it names a saved file.
            let disposition = loaded
                .reply
                .headers
                .iter()
                .position(|(name, _)| name == "content-disposition")
                .map(|at| loaded.reply.headers.swap_remove(at).1);
            let made = renderer.navigated(
                loaded.url,
                loaded.reply.status,
                content_type.as_deref(),
                loaded.reply.body,
                want,
                stopped,
            )?;
            let mut doc = match made {
                Ok(Made::Document(doc)) => doc,
                Ok(Made::Styles(styling)) => {
                    let _ = told.styles.send(());
                    subresource::gather(
                        &sheets.fetcher,
                        renderer,
                        &sheets.slots,
                        styling,
                        came + sheets.wait,
                        stopped,
                    )?
                }
                Err(retry) => {
                    // The window lets the page shown go before it answers.
                    let released = told.room.send(()).is_ok() && told.released.recv() == Ok(true);
                    if released {
                        renderer.retry(retry)?
                    } else {
                        retry.refused()
                    }
                }
            };
            doc.disposition = disposition;
            Some((doc, loaded.fragment.or(fragment)))
        }
        Err(why) => Some((renderer.error(Some(url), &why, want), fragment)),
    }
}

fn said(text: fmt::Arguments<'_>) -> Option<String> {
    td_alloc::format(text).ok()
}

/// `url` as the status row names what a save took: without its scheme,
/// at most `MAX_SAVED_URL` characters.
fn short(url: &Url) -> Option<String> {
    let text = url.text();
    let bare = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
        .unwrap_or(&text);
    match bare.char_indices().nth(MAX_SAVED_URL) {
        Some((cut, _)) => said(format_args!("{}…", bare.get(..cut).unwrap_or(bare))),
        None => said(format_args!("{bare}")),
    }
}

/// The most characters of a URL the status row names a save by.
const MAX_SAVED_URL: usize = 60;

/// Saves into `directory` the body `held` of a reply td-web does not
/// show, with its `content-disposition`, else `url` fetched again as a
/// reload fetches it; what the status row says once it has run, none
/// once `state` says it was stopped before it began to write.
fn saved(
    fetcher: &dyn fetch::Fetcher,
    directory: &File,
    url: Url,
    held: Option<(crate::memory::Resident<Vec<u8>>, Option<String>)>,
    state: &AtomicU8,
) -> Option<String> {
    let fetched;
    let (name, body, from) = match &held {
        Some((body, disposition)) => (save::name(disposition.as_deref(), &url), &***body, url),
        None => match fetch::navigate(fetcher, url) {
            Ok(loaded) if (200..300).contains(&loaded.reply.status) => {
                let name = save::name(loaded.reply.header("content-disposition"), &loaded.url);
                fetched = loaded.reply.body;
                (name, fetched.as_slice(), loaded.url)
            }
            Ok(loaded) => {
                return said(format_args!(
                    "not saved: the server answered {}",
                    loaded.reply.status
                ))
            }
            Err(why) => return said(format_args!("not saved: {why}")),
        },
    };
    // Stopped while it fetched: nothing is written. From here on the
    // window waits for the write rather than dropping it.
    if state
        .compare_exchange(FETCHING, WRITING, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
        && state.load(Ordering::SeqCst) != WRITING
    {
        return None;
    }
    let Ok(name) = name else {
        return said(format_args!("{}", save::Refusal::Memory));
    };
    let from = short(&from).unwrap_or_default();
    match save::store(directory, &name, body) {
        Ok(save::Stored {
            name,
            unsynced: None,
        }) => said(format_args!(
            "saved {} bytes of {from} in Downloads as {name}",
            body.len()
        )),
        Ok(save::Stored {
            name,
            unsynced: Some(why),
        }) => said(format_args!(
            "saved {} bytes of {from} in Downloads as {name}, but Downloads could not be \
             synced: {why}",
            body.len()
        )),
        Err(why) => said(format_args!("{why}")),
    }
}

impl App {
    pub fn new(
        fetcher: fetch::Shared,
        surface: Surface,
        faces: Option<Faces>,
    ) -> Result<Self, String> {
        let entry = |limit| EntryModel::new(limit).map_err(|why| format!("{why:?}"));
        let metrics = Metrics::new(faces.as_ref(), surface.scale);
        let renderer = Renderer::start(faces)?;
        let slots = Slots::new(renderer.budget()).map_err(|_| {
            td_alloc::string("no memory for the subresource slots").unwrap_or_default()
        })?;
        let scratch = Scratch::new_empty();
        Ok(Self {
            fetcher,
            renderer,
            workers: Arc::new(AtomicUsize::new(0)),
            slots,
            style_wait: subresource::STYLE_WAIT,
            picture_wait: pictures::PICTURE_WAIT,
            picture_slots: pictures::PICTURE_SLOTS,
            pictures_term: crate::memory::PICTURES,
            fit_work: pictures::FIT_WORK,
            surface,
            focus: Focus::Address,
            address: entry(MAX_ADDRESS)?,
            query: entry(MAX_QUERY)?,
            current: Current::new(Document::blank(), None),
            back: Vec::new(),
            forward: Vec::new(),
            loading: None,
            message: None,
            saving: None,
            home: None,
            pointer: None,
            hover: None,
            hover_image: None,
            title: "td-web".into(),
            redraw: true,
            quit: false,
            show_keys: false,
            metrics,
            serif: false,
            press: None,
            click: None,
            clock: Clock::new(),
            drag_scrolled: None,
            scratch,
        })
    }

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

    fn inset(&self) -> i64 {
        CELL_WIDTH as i64 * self.scale()
    }

    /// The pane a layout made at `scale` is shown in.
    fn pane_at(&self, scale: u8) -> Pane {
        let scale = i64::from(scale.max(1));
        Pane {
            rect: self.page_rect(),
            inset: CELL_WIDTH as i64 * scale,
            scale,
            line: i64::from(self.metrics.line().height.max(1)),
        }
    }

    /// The pane the shown layout is in.
    fn pane(&self) -> Pane {
        let scale = self
            .current
            .laid()
            .map_or(self.surface.scale.value(), |laid| {
                usize::from(laid.want.scale)
            });
        let scale = u8::try_from(scale).unwrap_or(1);
        self.pane_at(scale)
    }

    fn body(&self) -> TextFamily {
        if self.serif {
            TextFamily::Serif
        } else {
            TextFamily::Sans
        }
    }

    /// What a page is laid for now: the pane less a cell's inset each
    /// side, never narrower than `MIN_WRAP_EMS` ems, in CSS px.
    fn want(&self) -> Want {
        let scale = self.scale().max(1);
        let width = i64::from(self.page_rect().width) - 2 * self.inset();
        let least = MIN_WRAP_EMS * i64::from(td_ui::prose::SIZE);
        let height = i64::from(self.page_rect().height);
        Want {
            width: ((width / scale).max(least)) as f64,
            height: ((height / scale).max(1)) as f64,
            scale: u8::try_from(self.surface.scale.value()).unwrap_or(1),
            body: self.body(),
            styles: self.current.styles,
            pictures: self.current.pictures.as_ref().map_or(0, Pictures::settled),
        }
    }

    // Navigation.

    fn go(&mut self, typed: &str) {
        match td_url::address(typed) {
            Ok(reference) => self.follow(reference),
            Err(why) => self.message = Some(why.into()),
        }
    }

    fn follow(&mut self, reference: Reference) {
        match reference {
            Reference::Fetch { url, fragment } => {
                let same = self.current.doc.url.as_ref() == Some(&url);
                if same && fragment.is_some() && self.loading.is_none() {
                    self.current.fragment = fragment;
                    self.show_fragment();
                    self.show_address();
                    self.focus = Focus::Page;
                } else {
                    self.load(url, fragment, Motion::New, None);
                }
            }
            Reference::Other { scheme } => {
                self.message = Some(
                    format!(
                        "td-web does not open {} addresses",
                        document::visible(&scheme)
                    )
                    .into(),
                );
            }
        }
    }

    /// Scrolls to the element the fragment names, else the top.
    fn show_fragment(&mut self) -> bool {
        let fragment = self.current.fragment.clone();
        let pane = self.pane();
        let current = &mut self.current;
        let Some(laid) = current.doc.page().and_then(|p| p.laid.as_ref()) else {
            return false;
        };
        let target = fragment
            .as_deref()
            .and_then(|name| laid.tree().target(name))
            .and_then(|node| view::target_place(laid, node));
        match target {
            Some(place) => {
                let at = text::line_of(laid, place);
                current.view.home();
                if let Some(line) = laid.lines().get(at) {
                    current.view.y = (line.y * pane.scale as f64).floor() as i64;
                }
                current.view.clamp(laid, &pane);
                true
            }
            None => {
                current.view.home();
                false
            }
        }
    }

    fn load(&mut self, url: Url, fragment: Option<String>, motion: Motion, top: Option<Anchor>) {
        self.load_restoring(Entry { url, fragment, top }, motion, false);
    }

    /// `load`; with `restores`, `entry` is the page let go for a picture's
    /// room, held again if this navigation does not start.
    fn load_restoring(&mut self, entry: Entry, motion: Motion, restores: bool) {
        let Entry { url, fragment, top } = entry;
        let refused = |app: &mut Self, url: Url, fragment: Option<String>, top| {
            if restores {
                app.current.held = Some(Entry { url, fragment, top });
            }
        };
        if self.workers.load(Ordering::SeqCst) >= MAX_WORKERS {
            self.message = Some("earlier pages are still loading; try again shortly".into());
            refused(self, url, fragment, top);
            return;
        }
        let (send, answer) = mpsc::channel();
        let (asked, room) = mpsc::channel();
        let (released, given) = mpsc::channel();
        let (styling, styles) = mpsc::channel();
        let channels = Told {
            send,
            room: asked,
            released: given,
            styles: styling,
        };
        let (Ok(renderer), Ok(slots)) = (self.renderer.try_clone(), self.slots.try_clone()) else {
            self.message = said(format_args!(
                "the page could not be started: memory exhausted"
            ))
            .map(Cow::Owned);
            refused(self, url, fragment, top);
            return;
        };
        let sheets = Sheets {
            fetcher: Arc::clone(&self.fetcher),
            slots,
            wait: self.style_wait,
        };
        let (target, named) = (url.clone(), fragment.clone());
        let stopped = Arc::new(AtomicBool::new(false));
        let told = Arc::clone(&stopped);
        // A navigation is laid out with the page's own styles, before
        // any of its pictures has come.
        let want = Want {
            styles: true,
            pictures: 0,
            ..self.want()
        };
        self.workers.fetch_add(1, Ordering::SeqCst);
        let worker = Worker(Arc::clone(&self.workers));
        let spawned = std::thread::Builder::new()
            .name("td-web fetch".into())
            .spawn(move || {
                let _worker = worker;
                let arrival = arrive(&sheets, &renderer, target, named, want, &told, &channels);
                // An answer the window no longer takes is let go on the
                // render thread.
                if let Err(mpsc::SendError(Some((doc, _)))) = channels.send.send(arrival) {
                    renderer.let_go(Dropped::Document(doc));
                }
            });
        if let Err(why) = spawned {
            self.message = Some(format!("the page could not be fetched: {why}").into());
            refused(self, url, fragment, top);
            return;
        }
        self.end_loading();
        self.loading = Some(Loading {
            url,
            fragment,
            motion,
            top,
            answer: Some(answer),
            stopped: Stopped(stopped),
            room,
            released,
            owed: None,
            restores,
            styles,
            styling: false,
        });
        // The navigation's page is made before the page shown is laid
        // again: a running relayout is cancelled, so the render thread
        // takes the reply next, and runs again once no navigation is out.
        // One that let the layout go runs on: the page is never left
        // unlaid while the navigation is out.
        let shown = self.current.laid().is_some();
        if let Some(relayout) = self.current.relayout.as_ref().filter(|_| shown) {
            relayout.cancel.store(true, Ordering::SeqCst);
            self.current.relay_after = true;
        }
        self.focus = Focus::Page;
        self.show_address();
    }

    fn settle(&mut self) {
        if self
            .loading
            .as_ref()
            .is_some_and(|loading| loading.room.try_recv().is_ok())
        {
            self.make_room();
        }
        if let Some(loading) = &mut self.loading {
            if loading.styles.try_recv().is_ok() {
                loading.styling = true;
            }
        }
        self.answer_room();
        let Some(loading) = &self.loading else {
            return;
        };
        let Some(answer) = &loading.answer else {
            return;
        };
        let (doc, fragment) = match answer.try_recv() {
            Ok(Some(arrival)) => arrival,
            Err(TryRecvError::Empty) => return,
            Ok(None) | Err(TryRecvError::Disconnected) => (
                self.renderer.error(
                    Some(loading.url.clone()),
                    "the fetch ended without an answer",
                    // A page of its own, before any picture of it.
                    Want {
                        pictures: 0,
                        ..self.want()
                    },
                ),
                loading.fragment.clone(),
            ),
        };
        let Some(loading) = self.loading.take() else {
            return;
        };
        let here = match self.current.held.take() {
            Some(held) => Some(held),
            None => self.entry(),
        };
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

    /// A navigated picture asks for room: the page or picture shown is
    /// let go on the render thread, the pane showing paper, before the
    /// fetch is told to try the picture once more.
    fn make_room(&mut self) {
        let shown = matches!(self.current.doc.body, Body::Page(_) | Body::Picture(_));
        if shown {
            let here = self.entry();
            let old = std::mem::replace(&mut self.current, Current::new(Document::blank(), None));
            self.let_go_current(old);
            self.current.held = here;
            self.press = None;
            self.click = None;
            self.hover = None;
            self.hover_image = None;
            self.redraw = true;
        }
        if let Some(loading) = &mut self.loading {
            loading.owed = Some(shown);
        }
        self.answer_room();
    }

    /// Answers a picture's ask for room once what was let go for it has
    /// left the window for the render thread's lane, so the picture is
    /// made again behind its drop (DESIGN.md §10, "Memory").
    fn answer_room(&mut self) {
        self.renderer.release();
        let Some(loading) = &mut self.loading else {
            return;
        };
        let Some(shown) = loading.owed else {
            return;
        };
        let mut shown = shown;
        if shown && self.renderer.holding() {
            // A render thread lost and not started again takes nothing:
            // the picture is refused rather than waiting on it.
            if !self.renderer.lost() {
                return;
            }
            shown = false;
        }
        loading.owed = None;
        let _ = loading.released.send(shown);
    }

    fn install(&mut self, doc: Document, fragment: Option<String>, top: Option<Anchor>) {
        self.title = match (&doc.title, &doc.url) {
            (Some(title), _) => format!("{title} - td-web"),
            (None, Some(url)) => format!("{} - td-web", url.text()),
            (None, None) => "td-web".into(),
        };
        // A press on the page before it is not on this one.
        self.press = None;
        self.click = None;
        self.hover = None;
        self.hover_image = None;
        let old = std::mem::replace(&mut self.current, Current::new(doc, fragment));
        // The page let go is dropped on the render thread.
        self.let_go_current(old);
        let pane = self.pane();
        // The anchor is used only where the same bytes gave the same tree.
        let revived = match (top, self.current.laid()) {
            (Some(anchor), Some(laid)) => {
                text::revive(laid, anchor.place).map(|place| Anchor { place, ..anchor })
            }
            _ => None,
        };
        match revived {
            Some(anchor) => {
                if let Some(laid) = self.current.laid() {
                    let laid = laid.try_clone();
                    if let Ok(laid) = laid {
                        self.current.view.restore(&laid, &pane, anchor);
                    }
                }
            }
            None => {
                self.show_fragment();
            }
        }
        self.start_pictures();
        self.relayout();
        self.show_address();
        // A pointer left still over where the page arrived.
        self.refresh_hover();
    }

    /// The pictures of the page just shown, to be fetched as slots free:
    /// a markup page's, laid with its own sheets or the user agent's.
    fn start_pictures(&mut self) {
        let Some(url) = self.current.doc.url.as_ref() else {
            return;
        };
        let Some(page) = self.current.doc.page() else {
            return;
        };
        if page.tree.source() != Source::Markup || page.rung == Rung::Text {
            return;
        }
        // Refused for memory, the pictures keep their placeholders.
        if let Ok(pictures) = Pictures::new(
            &page.tree,
            url,
            &page.budget,
            self.pictures_term,
            self.fit_work,
        ) {
            self.current.pictures = pictures;
        }
    }

    /// The page's pictures: those come taken, those wanted asked for while
    /// no navigation is out (a page's sheets come first), and those come
    /// laid at most once every `PICTURE_SETTLE`, never while a relayout
    /// runs, so a long relayout is not cancelled by each settle.
    fn pictures(&mut self) {
        let now = self.clock.now();
        let Some(pictures) = self.current.pictures.as_mut() else {
            return;
        };
        if pictures.take() {
            self.redraw = true;
        }
        // While a navigation is out none is asked for and none laid: its
        // page is made first, and so its sheets fetched first.
        if self.loading.is_some() {
            return;
        }
        let base = if pictures.wanted() { self.base() } else { None };
        let Some(pictures) = self.current.pictures.as_mut() else {
            return;
        };
        if let (Some(base), Some(page)) = (&base, self.current.doc.page()) {
            let asker = pictures::Asker {
                fetcher: &self.fetcher,
                renderer: &self.renderer,
                slots: &self.slots,
                tree: &page.tree,
                base,
                wait: self.picture_wait,
                most: self.picture_slots,
            };
            pictures.ask(&asker);
        }
        let due = pictures.due(now) && self.current.relayout.is_none() && !self.current.again;
        if due {
            pictures.settle();
            self.relayout();
            self.redraw = true;
        }
    }

    /// The page's pictures let go on the render thread, their fetches
    /// told first.
    fn let_go_pictures(&mut self) {
        if let Some(pictures) = self.current.pictures.take() {
            pictures.leave();
            self.renderer.let_go(Dropped::Pictures(pictures));
        }
    }

    fn entry(&self) -> Option<Entry> {
        Some(Entry {
            url: self.current.doc.url.clone()?,
            fragment: self.current.fragment.clone(),
            top: self
                .anchor()
                .or_else(|| self.current.relayout.as_ref().and_then(|r| r.top)),
        })
    }

    /// The top's place in the layout shown.
    fn anchor(&self) -> Option<Anchor> {
        let laid = self.current.laid()?;
        self.current.view.anchor(laid, &self.pane())
    }

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

    /// The page shown fetched again; on the paper a page left for a
    /// picture's room, that page, its entry held until the reload ends.
    fn reload(&mut self) {
        if let Some(held) = self.current.held.take() {
            self.load_restoring(held, Motion::Reload, true);
        } else if let Some(entry) = self.entry() {
            self.load(entry.url, entry.fragment, Motion::Reload, entry.top);
        }
    }

    /// The navigation out stopped, and its answer, with whatever it holds
    /// or is yet sent it, let go on the render thread.
    /// A reload of the page let go for a picture's room gives its entry
    /// back to be held.
    fn end_loading(&mut self) -> bool {
        let Some(loading) = self.loading.take() else {
            return false;
        };
        let Loading {
            url,
            fragment,
            top,
            answer,
            stopped,
            restores,
            ..
        } = loading;
        stopped.0.store(true, Ordering::SeqCst);
        if let Some(answer) = answer {
            self.renderer.let_go(Dropped::Arrival(answer));
        }
        if restores {
            self.current.held = Some(Entry { url, fragment, top });
        }
        true
    }

    /// The page shown and a relayout of it let go on the render thread,
    /// an answer the relayout already made among them.
    fn let_go_current(&self, old: Current) {
        let Current {
            doc,
            relayout,
            pictures,
            ..
        } = old;
        if let Some(pictures) = pictures {
            pictures.leave();
            self.renderer.let_go(Dropped::Pictures(pictures));
        }
        if let Some(relayout) = relayout {
            // Cancelled and taken under the slot's lock, which the render
            // thread writes under: what it makes after is dropped there.
            let answered = {
                let mut slot = relayout
                    .answer
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                relayout.cancel.store(true, Ordering::SeqCst);
                slot.take()
            };
            if let Some(Ok(page)) = answered {
                self.renderer.let_go(Dropped::Page(page));
            }
        }
        self.renderer.let_go(Dropped::Document(doc));
    }

    fn stop(&mut self) -> bool {
        let restoring = self.loading.as_ref().is_some_and(|l| l.restores);
        if self.end_loading() {
            // Stopped on the paper a page left for a picture's room: that
            // page is shown again, unless that is what was stopped.
            if self.current.held.is_some() && !restoring {
                self.reload();
                return true;
            }
            self.message = Some("stopped".into());
            self.show_address();
            return true;
        }
        // Then the page's pictures still to come; those come stay.
        if self.current.pictures.as_mut().is_some_and(Pictures::stop) {
            self.message = Some("pictures stopped".into());
            return true;
        }
        // A save that has answered says what became of it, a failure
        // included, rather than being called stopped.
        self.settle_save();
        let Some(saving) = &self.saving else {
            return false;
        };
        let stopped = saving
            .state
            .compare_exchange(FETCHING, STOPPED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if stopped {
            // Its worker keeps its slot until its fetch returns.
            self.saving = None;
            self.message = said(format_args!("save stopped: nothing was written")).map(Cow::Owned);
        }
        stopped
    }

    /// What closing the window waits for: a save writing its file is
    /// waited for, its write and syncs unbounded in time; one still
    /// fetching is dropped and writes nothing.
    fn finish_saving(&mut self) {
        let Some(saving) = self.saving.take() else {
            return;
        };
        let dropped = saving
            .state
            .compare_exchange(FETCHING, STOPPED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if !dropped {
            let _ = saving.thread.join();
        }
    }

    /// Saves the document shown into Downloads (DESIGN.md §5, "Saving")
    /// on a worker's thread, once Downloads is found writable: the body
    /// kept of a reply td-web does not show, else the document fetched
    /// again, as a reload fetches it.
    fn save(&mut self) {
        if self.saving.is_some() {
            self.message = said(format_args!("a save is under way")).map(Cow::Owned);
            return;
        }
        let doc = &self.current.doc;
        if doc.url.is_none() {
            self.message =
                said(format_args!("nothing to save: this page is td-web's own")).map(Cow::Owned);
            return;
        }
        if doc.status == 0 {
            self.message =
                said(format_args!("not saved: the page could not be fetched")).map(Cow::Owned);
            return;
        }
        if !(200..300).contains(&doc.status) {
            self.message = said(format_args!(
                "not saved: the server answered {}",
                doc.status
            ))
            .map(Cow::Owned);
            return;
        }
        if self.workers.load(Ordering::SeqCst) >= MAX_WORKERS {
            self.message = said(format_args!(
                "earlier pages are still loading; try again shortly"
            ))
            .map(Cow::Owned);
            return;
        }
        let directory = match save::downloads(self.home.as_deref()) {
            Ok(directory) => directory,
            Err(why) => {
                self.message = said(format_args!("{why}")).map(Cow::Owned);
                return;
            }
        };
        let Some(Entry { url, .. }) = self.entry() else {
            return;
        };
        // A kept body is given up here: a save that then fails, or never
        // starts, leaves the next one to fetch it again.
        let doc = &mut self.current.doc;
        let held = doc.take_held().map(|body| (body, doc.disposition.take()));
        // A kept body has nothing to fetch: it is writing from the start.
        let begun = if held.is_some() { WRITING } else { FETCHING };
        let state = Arc::new(AtomicU8::new(begun));
        let told = Arc::clone(&state);
        let (send, answer) = mpsc::channel();
        let fetcher = Arc::clone(&self.fetcher);
        self.workers.fetch_add(1, Ordering::SeqCst);
        let worker = Worker(Arc::clone(&self.workers));
        let spawned = std::thread::Builder::new()
            .name("td-web save".into())
            .spawn(move || {
                let _worker = worker;
                let _ = send.send(saved(fetcher.as_ref(), &directory, url, held, &told));
            });
        match spawned {
            Ok(thread) => {
                self.saving = Some(Saving {
                    answer,
                    state,
                    thread,
                })
            }
            Err(why) => self.message = said(format_args!("not saved: {why}")).map(Cow::Owned),
        }
    }

    /// Takes a finished save's answer into the status row.
    fn settle_save(&mut self) {
        let Some(saving) = &self.saving else {
            return;
        };
        let answered = match saving.answer.try_recv() {
            Ok(answered) => answered,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                said(format_args!("not saved: the save ended without an answer"))
            }
        };
        self.saving = None;
        self.message = answered.map(Cow::Owned);
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

    /// Asks the render thread to lay the shown page out again when what
    /// it is laid for has changed: one at a time, a newer one cancelling
    /// the one running and waiting for it.
    fn relayout(&mut self) {
        let top = self.anchor();
        self.relayout_from(top);
    }

    /// The focused link's element in the layout shown.
    fn focused(&self) -> Option<NodeId> {
        let at = self.current.view.focus?;
        self.current.laid()?.links().get(at).map(|&(node, _)| node)
    }

    /// `relayout` keeping `top` at the top: the top's place shown, or the
    /// one a relayout that let the layout go took with it.
    fn relayout_from(&mut self, top: Option<Anchor>) {
        if self.current.relay_after {
            // Set only by a navigation that cancelled a relayout kept
            // beside the layout shown, which stays shown meanwhile.
            if self.loading.is_some() {
                self.current.again = true;
                return;
            }
            self.current.relay_after = false;
        }
        if self.metrics.scale() != self.surface.scale {
            self.metrics = Metrics::new(self.renderer.faces(), self.surface.scale);
        }
        let want = self.want();
        let focused = self.focused();
        let Some(page) = self.current.doc.page() else {
            self.current.again = false;
            return;
        };
        let pictures = self
            .current
            .pictures
            .as_ref()
            .map(|pictures| pictures.set(&page.budget))
            .transpose();
        if let Some(running) = &self.current.relayout {
            // The one running is for this unless it was cancelled; else it
            // is cancelled and another asked once it ends.
            let current = running.want == want && !running.cancel.load(Ordering::SeqCst);
            if !current {
                running.cancel.store(true, Ordering::SeqCst);
            }
            self.current.again = !current;
            return;
        }
        let laid = page.laid.as_ref();
        let refitting = self.current.refitting;
        if !refitting
            && laid.is_some_and(|laid| pipeline::redo(laid.want, want) == pipeline::Redo::Nothing)
        {
            // Back to what is shown: nothing was refused it.
            self.current.again = false;
            self.current.kept = false;
            return;
        }
        self.current.again = false;
        let root = self.renderer.budget();
        let headroom = root.limit().saturating_sub(root.used());
        // None shown: a relayout that let it go first was cancelled.
        let keep = laid.is_some_and(|laid| pipeline::fits_beside(laid, want, headroom));
        let parts = self.renderer.relayout_parts();
        let Some(page) = self.current.doc.page_mut() else {
            return;
        };
        let (Ok(tree), Ok(budget), Ok((faces, cancel, answer)), Ok(pictures)) = (
            page.tree.try_clone(),
            page.budget.try_clone(),
            parts,
            pictures,
        ) else {
            // Asked again at the next poll.
            self.message = Some(Cow::Borrowed(
                "the page could not be laid out again: memory",
            ));
            self.current.again = true;
            return;
        };
        let markup = page.markup.as_ref().and_then(|m| m.try_clone().ok());
        let sheets = page.sheets.as_ref().and_then(|s| s.try_clone().ok());
        // Kept beside: a reference goes with the request. Else the window
        // gives its own up and paints paper until the new one comes.
        let old = if keep {
            page.laid.as_ref().and_then(|laid| laid.try_clone().ok())
        } else {
            page.laid.take()
        };
        let again = Again {
            tree,
            markup,
            sheets,
            rung: page.rung,
            notes: page.notes,
            budget,
            old,
            keep,
            want,
            pictures,
        };
        let (Ok(theirs), Ok(answered)) = (cancel.try_clone(), answer.try_clone()) else {
            if let Some(page) = self.current.doc.page_mut() {
                if page.laid.is_none() {
                    page.laid = again.old;
                }
            }
            self.current.again = true;
            return;
        };
        let item = Lane::Again {
            again,
            faces,
            cancel: theirs,
            answer: answered,
        };
        match self.renderer.post(item) {
            Ok(()) => {
                self.current.refitting = false;
                self.current.relayout = Some(Relayout {
                    answer,
                    cancel,
                    top,
                    focused,
                    want,
                    let_go: !keep,
                });
            }
            Err(item) => {
                // A full lane: given back, the old layout kept, asked
                // again at the next poll.
                if let Lane::Again { again, .. } = item {
                    if let Some(page) = self.current.doc.page_mut() {
                        if page.laid.is_none() {
                            page.laid = again.old;
                        }
                    }
                }
                self.current.again = true;
            }
        }
    }

    /// Takes a finished relayout: the new layout installed and the old one
    /// sent back to be dropped on the render thread; a refusal keeps the
    /// old one, or with it let go, the page's next rung. The top's place,
    /// the focused link, the selection and find's current match are kept
    /// (DESIGN.md §8).
    fn settle_relayout(&mut self) {
        let Some(running) = &self.current.relayout else {
            if self.current.again {
                self.relayout();
            }
            return;
        };
        let taken = running
            .answer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(made) = taken else {
            return;
        };
        let Some(running) = self.current.relayout.take() else {
            return;
        };
        self.redraw = true;
        let now = self.clock.now();
        if let Some(pictures) = self.current.pictures.as_mut() {
            pictures.laid(now);
        }
        match made {
            Ok(page) => {
                let focused = self.focused().or(running.focused);
                // Where the reader is now, the old layout still shown;
                // where they were when it was asked, it let go.
                let top = self.anchor().or(running.top);
                let Some(shown) = self.current.doc.page_mut() else {
                    self.renderer.let_go(Dropped::Page(page));
                    return;
                };
                // A step down makes another tree: its places are not the
                // old one's.
                let same = BudgetShared::ptr_eq(&shown.tree, &page.tree);
                let old = std::mem::replace(shown, page);
                // Copies refused while the old layout held its own are
                // made again, once a want, the layout shown kept beside:
                // the old goes to the lane before the relayout does, so
                // its copies' room is had.
                let short = shown.laid.as_ref().map_or(0, |laid| laid.short());
                let copies = old.laid.as_ref().map_or(0, |laid| laid.copies());
                let want = shown.laid.as_ref().map(|laid| laid.want);
                self.renderer.let_go(Dropped::Page(old));
                if short > 0 && copies > 0 && want.is_some() && self.current.refit != want {
                    self.current.refit = want;
                    self.current.refitting = true;
                    self.current.again = true;
                }
                self.current.kept = false;
                self.current.measure = None;
                let pane = self.pane();
                if let Some(laid) = self.current.laid().and_then(|laid| laid.try_clone().ok()) {
                    let view = &mut self.current.view;
                    if same {
                        view.focus = focused.and_then(|node| view::link_index(&laid, node));
                        match top {
                            Some(top) => view.restore(&laid, &pane, top),
                            None => view.clamp(&laid, &pane),
                        }
                    } else {
                        view.focus = None;
                        view.hits = None;
                        view.at = 0;
                        view.selection = None;
                        view.clamp(&laid, &pane);
                    }
                }
                // Stepped down to another tree, rung 3's: no picture of
                // the old one is in it.
                if !same {
                    self.let_go_pictures();
                }
                if same && self.current.view.hits.is_some() && !self.current.find_refused {
                    self.remark();
                }
                self.refresh_hover();
            }
            Err(Refusal::Cancelled) => {
                if running.let_go || self.current.relay_after {
                    // Nothing to show until the one after it comes.
                    self.current.again = true;
                }
            }
            Err(Refusal::Limit(_)) if !running.let_go => self.current.kept = true,
            Err(Refusal::Limit(_)) => {
                let old = std::mem::replace(&mut self.current.doc.body, Body::Resources);
                if let Body::Page(page) = old {
                    self.renderer.let_go(Dropped::Page(page));
                }
                self.let_go_pictures();
            }
        }
        if self.current.again || running.let_go && self.current.laid().is_none() {
            let top = self.anchor().or(running.top);
            self.relayout_from(top);
        }
    }

    /// Find's matches made again on a new layout, which can lay text the
    /// old one did not: the current match's index kept as far as there
    /// are as many, nothing moved to or selected. A refusal keeps the old
    /// ones, places that outlive a relayout.
    fn remark(&mut self) {
        let Some(laid) = self.current.laid().and_then(|laid| laid.try_clone().ok()) else {
            return;
        };
        let Some(memory) = self
            .current
            .doc
            .page()
            .and_then(|page| page.budget.try_clone().ok())
        else {
            return;
        };
        if let Ok(hits) = crate::search::find(&laid, self.query.text(), &memory) {
            let view = &mut self.current.view;
            view.at = view.at.min(hits.len().saturating_sub(1));
            view.hits = Some(hits);
        }
    }

    fn refind(&mut self) {
        let query = self.query.text().to_string();
        let pane = self.pane();
        let Some(laid) = self.current.laid().and_then(|laid| laid.try_clone().ok()) else {
            return;
        };
        let memory = match self.current.doc.page() {
            Some(page) => page.budget.try_clone(),
            None => return,
        };
        let Ok(memory) = memory else {
            self.current.find_refused = true;
            return;
        };
        match crate::search::find(&laid, &query, &memory) {
            Ok(hits) => {
                self.current.find_refused = false;
                let none = hits.is_empty();
                self.current.view.hits = Some(hits);
                if none {
                    // A query that matches nothing leaves no selection.
                    self.current.view.at = 0;
                    self.current.view.selection = None;
                    return;
                }
                let first = self.current.view.first_hit_shown(&laid, &pane);
                self.current.view.at = first;
                if let Some((_, measure)) =
                    measure_of(&self.current.doc, &mut self.current.measure, &self.renderer)
                {
                    let view = &mut self.current.view;
                    if let Some(hit) = view.hits().get(view.at).copied() {
                        select(
                            view,
                            Some(Selection {
                                anchor: hit.start,
                                focus: hit.end,
                            }),
                        );
                        view.reveal(&laid, measure, &pane, hit.start);
                    }
                }
            }
            Err(_) => self.current.find_refused = true,
        }
        self.refresh_hover();
    }

    fn follow_link(&mut self, link: usize) {
        match self.link_target(link) {
            Some(Ok(reference)) => self.follow(reference),
            Some(Err(why)) => {
                self.message = Some(format!("the link is not followed: {why}").into())
            }
            None => {}
        }
    }

    /// The base links are joined against: the first `<base href>` joined
    /// against the page's URL, else that URL.
    fn base(&self) -> Option<Url> {
        let url = self.current.doc.url.as_ref()?;
        let base = self
            .current
            .doc
            .page()
            .and_then(|page| page.tree.base())
            .and_then(|href| url.join(href).ok());
        Some(base.unwrap_or_else(|| url.clone()))
    }

    /// Where laid link `link` goes.
    fn link_target(&self, link: usize) -> Option<Result<Reference, String>> {
        let laid = self.current.laid()?;
        let &(node, _) = laid.links().get(link)?;
        let href = laid.tree().href(node)?;
        let base = self.base()?;
        Some(td_url::resolve(&base, href.trim()))
    }

    /// Opens an image's `src`, as a click on an image with no link does.
    fn follow_src(&mut self, image: td_html::document::NodeId) {
        let target = (|| {
            let laid = self.current.laid()?;
            let src = laid
                .tree()
                .document()
                .node(image)?
                .element()?
                .attribute("src")?;
            let base = self.base()?;
            Some(td_url::resolve(&base, src.trim()))
        })();
        match target {
            Some(Ok(reference)) => self.follow(reference),
            Some(Err(why)) => {
                self.message =
                    said(format_args!("the picture is not opened: {why}")).map(Cow::Owned)
            }
            None => {}
        }
    }

    /// A link as the status row shows it.
    fn link_shown(&self, link: usize) -> Option<String> {
        match self.link_target(link)? {
            Ok(Reference::Fetch { url, fragment }) => Some(match fragment {
                Some(fragment) => format!("{}#{}", url.text(), document::visible(&fragment)),
                None => url.text(),
            }),
            // Another scheme, or an href that does not resolve: as
            // written.
            Ok(Reference::Other { .. }) | Err(_) => {
                let laid = self.current.laid()?;
                let &(node, _) = laid.links().get(link)?;
                let href = laid.tree().href(node)?;
                Some(document::visible(href.trim()))
            }
        }
    }

    fn scroll(&mut self, rows: isize) {
        let pane = self.pane();
        let Some(laid) = self.current.laid().and_then(|laid| laid.try_clone().ok()) else {
            return;
        };
        let dy = (rows as i64).saturating_mul(pane.line);
        self.current.view.scroll(&laid, &pane, dy, 0);
        self.refresh_hover();
    }

    fn scroll_across(&mut self, columns: i64) {
        let pane = self.pane();
        let Some(laid) = self.current.laid().and_then(|laid| laid.try_clone().ok()) else {
            return;
        };
        let dx = columns.saturating_mul(CELL_WIDTH as i64 * pane.scale);
        self.current.view.scroll(&laid, &pane, 0, dx);
    }

    fn refresh_hover(&mut self) {
        self.hover = self.pointer.and_then(|(x, y)| self.link_at(x, y));
        self.hover_image = match (self.hover, self.pointer) {
            (None, Some((x, y))) => self.image_at(x, y),
            _ => None,
        };
    }

    /// The image in no link under a window point, which a click opens.
    fn image_at(&mut self, x: i64, y: i64) -> Option<NodeId> {
        let pane = self.pane();
        if !pane.rect.contains(x, y) {
            return None;
        }
        let (laid, measure) =
            measure_of(&self.current.doc, &mut self.current.measure, &self.renderer)?;
        let laid = laid.try_clone().ok()?;
        let (_, item) = self.current.view.spot(&laid, measure, &pane, x, y)?;
        unlinked_image(&laid, item)
    }

    /// Where an image in no link opens: its `src`, joined against the
    /// page's base, or as written when it does not resolve.
    fn src_shown(&self, image: NodeId) -> Option<String> {
        let laid = self.current.laid()?;
        let src = laid
            .tree()
            .document()
            .node(image)?
            .element()?
            .attribute("src")?;
        match td_url::resolve(&self.base()?, src.trim()) {
            Ok(Reference::Fetch { url, .. }) => Some(url.text()),
            _ => Some(document::visible(src.trim())),
        }
    }

    fn page_key(&mut self, chord: &str) {
        if self.current.find_refused && matches!(chord, "n" | "N" | "S-n") {
            self.refind();
            return;
        }
        let pane = self.pane();
        let page = ((pane.height() - pane.line).max(pane.line) / pane.line.max(1)) as isize;
        match chord {
            "Up" => return self.scroll(-1),
            "Down" => return self.scroll(1),
            "Left" => return self.scroll_across(-ACROSS),
            "Right" => return self.scroll_across(ACROSS),
            "PageUp" | "S-space" => return self.scroll(-page),
            "PageDown" | "space" => return self.scroll(page),
            "/" => return self.open_find(),
            "?" => {
                self.show_keys = true;
                return;
            }
            "S-Left" | "S-Right" | "C-S-Left" | "C-S-Right" | "S-Up" | "S-Down" => {
                return self.extend_by(chord);
            }
            "Return" => {
                if let Some(link) = self.current.view.focus {
                    self.follow_link(link);
                }
                return;
            }
            "Escape" => {
                if !self.stop() {
                    self.current.view.focus = None;
                    self.current.view.hits = None;
                    self.current.find_refused = false;
                    self.current.view.selection = None;
                }
                return;
            }
            _ => {}
        }
        let Some((laid, measure)) =
            measure_of(&self.current.doc, &mut self.current.measure, &self.renderer)
        else {
            return;
        };
        let Ok(laid) = laid.try_clone() else {
            return;
        };
        let view = &mut self.current.view;
        match chord {
            "Home" | "C-Home" => view.home(),
            "End" | "C-End" => view.end(&laid, &pane),
            // A focused link lets the selection go.
            "Tab" | "S-Tab" => {
                view.selection = None;
                view.step_link(&laid, measure, &pane, chord == "Tab");
            }
            "n" | "N" | "S-n" => step_hit(view, &laid, measure, &pane, chord == "n"),
            "C-a" => {
                let (start, end) = text::page_bounds(&laid);
                select(
                    view,
                    Some(Selection {
                        anchor: start,
                        focus: end,
                    }),
                );
            }
            _ => return,
        }
        self.refresh_hover();
    }

    fn toggle_body(&mut self) {
        self.serif = !self.serif;
        self.relayout();
        let family = if self.serif { "Serif" } else { "Sans" };
        self.message = Some(Cow::Owned(if self.metrics.proportional() {
            format!("body text in Noto {family}")
        } else {
            format!("body text in Noto {family}, which is not loaded: the page draws on the cell")
        }));
    }

    /// `F9`: the page without its own styles, or with them again.
    fn toggle_styles(&mut self) {
        let Some(page) = self.current.doc.page() else {
            self.message = Some(Cow::Borrowed("F9 shows a page without its styles"));
            return;
        };
        if page.tree.source() != Source::Markup || page.rung != Rung::Own {
            self.message = Some(Cow::Borrowed("this page shows no styles of its own"));
            return;
        }
        self.current.styles = !self.current.styles;
        self.message = Some(Cow::Borrowed(if self.current.styles {
            "the page's own styles"
        } else {
            "the page without its own styles"
        }));
        self.relayout();
    }

    fn open_find(&mut self) {
        self.focus = Focus::Find;
        let _ = self.query.act(Action::SelectAll);
    }

    fn extend_by(&mut self, chord: &str) {
        let pane = self.pane();
        let Some((laid, measure)) =
            measure_of(&self.current.doc, &mut self.current.measure, &self.renderer)
        else {
            return;
        };
        let Ok(laid) = laid.try_clone() else {
            return;
        };
        if laid.lines().is_empty() {
            return;
        }
        let view = &mut self.current.view;
        let selection = view.selection.unwrap_or_else(|| {
            let start = view
                .focus
                .and_then(|link| {
                    let &(node, line) = laid.links().get(link)?;
                    view::first_place(&laid, node, line as usize)
                })
                .or_else(|| view.anchor(&laid, &pane).map(|a| a.place))
                .unwrap_or_default();
            Selection {
                anchor: start,
                focus: start,
            }
        });
        let motion = match chord {
            "S-Left" => Move::Left,
            "S-Right" => Move::Right,
            "C-S-Left" => Move::WordLeft,
            "C-S-Right" => Move::WordRight,
            "S-Up" => Move::Up,
            _ => Move::Down,
        };
        let focus = text::moved(&laid, measure, selection.focus, motion);
        view.reveal(&laid, measure, &pane, focus);
        select(view, Some(Selection { focus, ..selection }));
        self.refresh_hover();
    }

    fn selected_text(&self) -> Option<Result<OwnedText, TextError>> {
        let (start, end) = self.current.view.selection?.range()?;
        let laid = self.current.laid()?;
        let memory = self
            .current
            .doc
            .page()
            .map_or(self.renderer.budget(), |page| &page.budget);
        let (bytes, past) = text::measure_copy(laid, start, end, MAX_COPY);
        if past {
            return Some(Err(TextError::TooLong));
        }
        if bytes == 0 {
            return None;
        }
        Some(match text::copied(laid, start, end, MAX_COPY, memory) {
            Ok(Some(text)) => OwnedText::copy(memory, &text),
            Ok(None) => Err(TextError::TooLong),
            Err(error) => Err(TextError::Resource(error)),
        })
    }

    fn copy(&mut self, clipboard: &mut dyn Clipboard) {
        let memory = self.renderer.budget();
        let entry = match self.focus {
            Focus::Address => self.address.selected_text().ok().flatten(),
            Focus::Find => self.query.selected_text().ok().flatten(),
            Focus::Page => None,
        };
        let selected = entry
            .map(|text| OwnedText::copy(memory, text))
            .or_else(|| self.selected_text());
        if let Some(text) = selected {
            self.offer_copy(text, clipboard);
            return;
        }
        let text = self
            .current
            .view
            .focus
            .and_then(|link| self.link_shown(link))
            .or_else(|| self.current.doc.url.as_ref().map(|url| url.text()));
        if let Some(text) = text {
            let text = OwnedText::copy(self.renderer.budget(), &text);
            self.offer_copy(text, clipboard);
        }
    }

    fn offer_copy(&mut self, text: Result<OwnedText, TextError>, clipboard: &mut dyn Clipboard) {
        self.message = Some(Cow::Borrowed(match text {
            Ok(text) => match clipboard.copy_owned(text) {
                Ok(()) => "copied",
                Err(_) => "not copied: the clipboard refused the selection",
            },
            Err(TextError::TooLong) => "not copied: the selection is past 1 MiB",
            Err(TextError::Resource(_)) => "not copied: memory limit",
        }));
    }

    fn edit(
        entry: &mut EntryModel,
        chord: &str,
        clipboard: &mut dyn Clipboard,
        memory: &td_alloc::Budget,
        notice: &mut Option<Cow<'static, str>>,
    ) -> bool {
        let action = match chord {
            "space" => Some(Action::Insert(' ')),
            "C-v" => {
                let _ = clipboard.paste();
                return true;
            }
            "C-x" => {
                if let Ok(Some(text)) = entry.selected_text() {
                    *notice = Some(Cow::Borrowed(match OwnedText::copy(memory, text) {
                        Ok(text) => match clipboard.copy_owned(text) {
                            Ok(()) => {
                                let _ = entry.act(Action::Delete);
                                "copied"
                            }
                            Err(_) => "not copied: the clipboard refused the selection",
                        },
                        Err(TextError::TooLong) => "not copied: the selection is past 1 MiB",
                        Err(TextError::Resource(_)) => "not copied: memory limit",
                    }));
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
            "C-s" => return self.save(),
            "F8" => return self.toggle_body(),
            "F9" => return self.toggle_styles(),
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
                    Self::edit(
                        &mut self.address,
                        chord,
                        clipboard,
                        self.renderer.budget(),
                        &mut self.message,
                    );
                }
            },
            Focus::Find => {
                if self.current.find_refused && matches!(chord, "Return" | "S-Return") {
                    self.refind();
                    return;
                }
                let pane = self.pane();
                let step = match chord {
                    "Return" => Some(true),
                    "S-Return" => Some(false),
                    _ => None,
                };
                if let Some(forward) = step {
                    if let Some((laid, measure)) =
                        measure_of(&self.current.doc, &mut self.current.measure, &self.renderer)
                    {
                        if let Ok(laid) = laid.try_clone() {
                            step_hit(&mut self.current.view, &laid, measure, &pane, forward);
                        }
                    }
                    self.refresh_hover();
                    return;
                }
                match chord {
                    "Escape" => self.focus = Focus::Page,
                    _ => {
                        if Self::edit(
                            &mut self.query,
                            chord,
                            clipboard,
                            self.renderer.budget(),
                            &mut self.message,
                        ) {
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
        if self.current.find_refused {
            return;
        }
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
        let Some((laid, measure)) =
            measure_of(&self.current.doc, &mut self.current.measure, &self.renderer)
        else {
            return;
        };
        let Ok(laid) = laid.try_clone() else {
            return;
        };
        let view = &mut self.current.view;
        let spot = view.spot(&laid, measure, &pane, x, y);
        let image = spot
            .and_then(|(_, item)| unlinked_image(&laid, item))
            .filter(|_| count == 1 && !extend);
        let top = view
            .anchor(&laid, &pane)
            .map_or(Place::default(), |a| a.place);
        let caret = spot.map_or(top, |(place, _)| place);
        let anchor = match view.selection {
            Some(selection) if extend => selection.anchor,
            _ => caret,
        };
        let selection = match count {
            _ if extend => Some(Selection {
                anchor,
                focus: caret,
            }),
            // The word of the scalar under the pointer, not the caret's.
            2 => scalar_under(&laid, measure, view, &pane, x, y)
                .and_then(|place| text::word(&laid, measure, place))
                .map(|(anchor, focus)| Selection { anchor, focus }),
            3 => spot
                .and_then(|(place, _)| text::paragraph_places(&laid, place))
                .map(|(anchor, focus)| Selection { anchor, focus }),
            _ => None,
        };
        select(view, selection);
        self.press = Some(Press {
            from: (x, y),
            at: (x, y),
            anchor,
            link,
            image,
            count,
            selecting: extend,
            moved: false,
        });
    }

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

    fn select_to(&mut self, x: i64, y: i64) {
        let Some(press) = self.press else {
            return;
        };
        let pane = self.pane();
        let Some((laid, measure)) =
            measure_of(&self.current.doc, &mut self.current.measure, &self.renderer)
        else {
            return;
        };
        let Ok(laid) = laid.try_clone() else {
            return;
        };
        let view = &mut self.current.view;
        if let Some((caret, _)) = view.spot(&laid, measure, &pane, x, y) {
            select(
                view,
                Some(Selection {
                    anchor: press.anchor,
                    focus: caret,
                }),
            );
        }
    }

    fn release(&mut self, x: i64, y: i64) {
        self.drag_scrolled = None;
        let Some(press) = self.press.take() else {
            return;
        };
        if press.selecting {
            return;
        }
        match (press.link, press.image) {
            (Some(link), _) => self.follow_link(link),
            (None, Some(image)) => self.follow_src(image),
            (None, None) => {
                self.click = Some(Click {
                    released: self.clock.now(),
                    at: (x, y),
                    count: press.count,
                })
            }
        }
    }

    /// Body lines a drag past the pane's top or bottom scrolls by.
    fn drag_past(&self) -> Option<i64> {
        let press = self.press.filter(|press| press.selecting && press.moved)?;
        let pane = self.pane();
        // A pane that shows no line whole does not scroll so.
        if pane.height() < pane.line {
            return None;
        }
        let height = pane.line.max(1);
        let top = pane.rect.y;
        let bottom = top + pane.height();
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
        let before = (self.current.view.y, self.current.view.selection);
        self.scroll(isize::try_from(lines).unwrap_or(0));
        self.reselect();
        if (self.current.view.y, self.current.view.selection) != before {
            self.redraw = true;
        }
    }

    fn reselect(&mut self) {
        if let Some(press) = self.press.filter(|press| press.selecting) {
            self.select_to(press.at.0, press.at.1);
        }
    }

    fn link_at(&mut self, x: i64, y: i64) -> Option<usize> {
        let pane = self.pane();
        let (laid, measure) =
            measure_of(&self.current.doc, &mut self.current.measure, &self.renderer)?;
        let laid = laid.try_clone().ok()?;
        self.current.view.link_at(&laid, measure, &pane, x, y)
    }

    fn status_text(&self) -> Cow<'_, str> {
        if let Some(message) = &self.message {
            return Cow::Borrowed(message);
        }
        if let Some(loading) = &self.loading {
            let styles = if loading.styling { "styles of " } else { "" };
            return said(format_args!("loading {styles}{}", loading.url.text()))
                .map_or(Cow::Borrowed("loading"), Cow::Owned);
        }
        if self.saving.is_some() {
            return Cow::Borrowed("saving in Downloads");
        }
        let link = self.hover.or(self.current.view.focus);
        if let Some(shown) = link.and_then(|link| self.link_shown(link)) {
            return shown.into();
        }
        if let Some(shown) = self.hover_image.and_then(|image| self.src_shown(image)) {
            return shown.into();
        }
        let doc = &self.current.doc;
        let mut out = String::new();
        if doc.status != 0 && !(200..300).contains(&doc.status) {
            add_part(&mut out, format_args!("status {}", doc.status));
        }
        if self.current.relayout.is_some() {
            add_part(&mut out, format_args!("laying out"));
        }
        if let Some(laid) = self.current.laid() {
            // Said only of a page not shown whole.
            let (bottom, lines) = self.current.view.bottom(laid, &self.pane());
            if bottom < lines || self.current.view.y > 0 {
                add_part(&mut out, format_args!("{bottom} of {lines} lines"));
            }
        }
        if let Some(note) = &doc.note {
            add_part(&mut out, format_args!("{note}"));
        }
        if let Some(page) = doc.page() {
            let notes = page.notes;
            if notes.omitted > 0 {
                let were = if notes.omitted == 1 { "was" } else { "were" };
                add_part(
                    &mut out,
                    format_args!(
                        "{} of the page's style sheets {were} not applied",
                        notes.omitted
                    ),
                );
            }
            if notes.late > 0 {
                let sheets = if notes.late == 1 { "sheet" } else { "sheets" };
                add_part(
                    &mut out,
                    format_args!(
                        "{} style {sheets} did not come in time; Ctrl+R asks again",
                        notes.late
                    ),
                );
            }
            let laid = self.current.laid();
            let drawn = |node| !laid.is_some_and(|laid| laid.refused(node));
            if let Some(count) = self.current.pictures.as_ref().map(|p| p.count(drawn)) {
                if count.out {
                    add_part(
                        &mut out,
                        format_args!("loading pictures, {} of {} shown", count.shown, count.of),
                    );
                } else if count.not > 0 {
                    let are = if count.not == 1 { "is" } else { "are" };
                    let again = if count.stopped {
                        "; Ctrl+R asks again"
                    } else {
                        ""
                    };
                    if count.of == 1 {
                        add_part(
                            &mut out,
                            format_args!("the page's picture is not shown{again}"),
                        );
                    } else {
                        add_part(
                            &mut out,
                            format_args!(
                                "{} of the page's {} pictures {are} not shown{again}",
                                count.not, count.of
                            ),
                        );
                    }
                }
            }
            if let Some(limit) = notes.styles {
                add_part(
                    &mut out,
                    format_args!("the page's styles were refused ({limit})"),
                );
            }
            if let Some(limit) = notes.text {
                add_part(
                    &mut out,
                    format_args!("the page was too large to lay out ({limit}); shown as text"),
                );
            }
            if let Some(shown) = notes.shown {
                let part = if shown.part { ", the last in part" } else { "" };
                add_part(
                    &mut out,
                    format_args!("{} of its {} lines are shown{part}", shown.lines, shown.of),
                );
            }
            // Of the layout shown, which a refused relayout keeps.
            let styles = page
                .laid
                .as_ref()
                .map_or(self.current.styles, |laid| laid.want.styles);
            if !styles {
                add_part(&mut out, format_args!("without the page's styles (F9)"));
            }
            if let Some(laid) = &page.laid {
                let cut = laid.layout.cuts.lines;
                if cut > 0 {
                    add_part(
                        &mut out,
                        format_args!(
                            "{cut} unwrapped lines are cut at {} characters",
                            td_layout::MAX_LINE_SCALARS
                        ),
                    );
                }
            }
        }
        if self.current.kept {
            add_part(
                &mut out,
                format_args!("the page could not be laid out again"),
            );
        }
        if self.current.cut {
            add_part(&mut out, format_args!("the page was painted in part"));
        }
        if doc.url.is_none() && matches!(doc.body, Body::Blank) {
            add_part(
                &mut out,
                format_args!("Ctrl+L for an address, F1 for the keys"),
            );
        }
        out.into()
    }
}

/// `part` after the status row's others; one memory refuses is left out.
fn add_part(out: &mut String, part: fmt::Arguments<'_>) {
    let Some(part) = said(part) else {
        return;
    };
    let gap = if out.is_empty() { "" } else { " · " };
    let _ = td_alloc::push_str(out, gap).and_then(|()| td_alloc::push_str(out, &part));
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
    ("Left, Right", "across a page wider than the pane"),
    ("Home, End", "top or bottom"),
    (
        "/, Ctrl+F",
        "find and select a match; n and N next and previous",
    ),
    (
        "drag, double or triple click",
        "select text, a word or a paragraph",
    ),
    ("Shift+click", "extend the selection"),
    (
        "Shift+arrows",
        "extend it a character or paragraph; with Ctrl a word",
    ),
    ("Ctrl+A", "select the page"),
    (
        "Escape",
        "stop loading or a save fetching, else clear focus, find and selection",
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
    ("Ctrl+S", "save the document in Downloads"),
    ("F8", "body text in Sans or Serif"),
    ("F9", "the page without its own styles, or with them"),
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
            Input::Wheel { rows, columns } => {
                self.message = None;
                self.scroll(rows);
                // A column across is a `Left` or `Right` press's cells.
                self.scroll_across((columns as i64).saturating_mul(ACROSS));
                self.refresh_hover();
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
        self.renderer.release();
        if self.loading.is_some() {
            self.settle();
            self.redraw = true;
        }
        self.pictures();
        if self.saving.is_some() {
            self.settle_save();
            self.redraw = true;
        }
        if self.current.relayout.is_some() || self.current.again {
            self.settle_relayout();
        }
        self.drag_scroll(now);
        self.flow()
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        if self.drag_past().is_some() {
            DRAG_SCROLL_MS
        } else if self.loading.is_some()
            || self.saving.is_some()
            || self.current.relayout.is_some()
            || self.current.again
            || self.current.pictures.as_ref().is_some_and(Pictures::busy)
        {
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
        match &self.current.doc.body {
            Body::Picture(picture) => {
                fill(page, PAPER, bounds, &mut sink);
                let (width, height) = picture.fit_size(page.width as usize, page.height as usize);
                let doc = &self.current.doc;
                let fitted = self
                    .current
                    .fitted
                    .get_or_fit(|| doc.fit_picture(width, height));
                if let Some(fitted) = fitted {
                    raster.image(page, page, fitted);
                } else {
                    notice(surface, page, bounds, &mut sink);
                }
            }
            Body::Resources => notice(surface, page, bounds, &mut sink),
            Body::Blank => fill(page, PAPER, bounds, &mut sink),
            Body::Page(shown) if shown.laid.is_none() => fill(page, PAPER, bounds, &mut sink),
            Body::Page(_) => {
                let pane = self.pane();
                let hover = self.hover.or(self.current.view.focus);
                let link = hover.and_then(|at| {
                    self.current
                        .laid()
                        .and_then(|laid| laid.links().get(at).map(|(node, _)| *node))
                });
                let _ = paint::reserve(&mut self.scratch, self.renderer.budget());
                let mut scratch = std::mem::replace(&mut self.scratch, Scratch::new_empty());
                let painted = match measure_of(
                    &self.current.doc,
                    &mut self.current.measure,
                    &self.renderer,
                ) {
                    Some((laid, measure)) => {
                        let laid = laid.try_clone().ok();
                        laid.map(|laid| {
                            let view = &self.current.view;
                            let marks = Marks {
                                hits: view.hits(),
                                selection: view.selection.and_then(Selection::range),
                                link,
                            };
                            let frame = view.frame(&pane);
                            // An inline picture is its layout's fitted copy.
                            let mut sink = |draw: Draw| match draw.primitive {
                                Primitive::Image { id, rect } => {
                                    if let Some((fitted, factor)) = laid.fitted_by(id) {
                                        raster.image_scaled(draw.clip, rect, fitted, factor);
                                    }
                                }
                                _ => raster.draw(draw),
                            };
                            paint::paint(
                                &laid,
                                measure,
                                &frame,
                                bounds,
                                &marks,
                                &mut scratch,
                                &mut sink,
                            )
                        })
                    }
                    None => None,
                };
                self.scratch = scratch;
                self.current.cut = painted.is_some_and(|p| p.cut);
            }
        }
        let status = self.status();
        let mut sink = |draw| raster.draw(draw);
        if self.focus == Focus::Find && self.message.is_none() {
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
        let hover = (self.hover, self.hover_image);
        self.refresh_hover();
        if hover != (self.hover, self.hover_image) {
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
    app.home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(start) = start {
        app.open(&start);
    }
    let ran =
        td_ui::window::run_with_prose(&mut app, stream, std::env::temp_dir(), typeface, prose)
            .map_err(|why| why.to_string());
    app.finish_saving();
    ran
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;
    use crate::memory;
    use std::sync::Mutex;
    use td_alloc::Budget;

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
                picture.fit(1, 1).and_then(|image| {
                    let budget = crate::memory::root().ok()?;
                    let bytes = image.bgra.capacity();
                    crate::memory::Resident::new(image, budget.charge(bytes).ok()?, bytes).ok()
                })
            })
            .is_none());
        assert!(!retried);
        cache = PictureFit::Empty;
        assert!(cache
            .get_or_fit(|| picture.fit(1, 1).and_then(|image| {
                let budget = crate::memory::root().ok()?;
                let bytes = image.bgra.capacity();
                crate::memory::Resident::new(image, budget.charge(bytes).ok()?, bytes).ok()
            }))
            .is_some());
    }

    use crate::fetch::tests::Table;
    use crate::fetch::Reply;
    use crate::fetch::{Ask, Fetcher};
    use std::ops::ControlFlow;
    use std::path::Path;

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
        fn copy_owned(&mut self, text: OwnedText) -> Result<(), td_ui::window::Refusal> {
            self.copies.push(text.as_str().to_string());
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
        fn get(&self, url: &str, ask: Ask) -> Result<Reply, String> {
            let _ = self.release.lock().unwrap().recv();
            self.table.get(url, ask)
        }
    }

    fn surface(width: usize) -> Surface {
        Surface::new(width, 600, Scale::new(1).unwrap()).unwrap()
    }

    /// The window over `fetcher`, its pages waiting for their sheets
    /// longer than any test but one about lateness takes, even in a debug
    /// build on a loaded host.
    fn app_over(fetcher: Arc<dyn Fetcher>) -> App {
        let mut app = App::new(fetcher, surface(800), None).unwrap();
        app.style_wait = std::time::Duration::from_secs(60);
        app
    }

    fn app(table: Table) -> App {
        app_over(Arc::new(table))
    }

    /// Waits for the fetch out to answer and takes it: a deadline rather
    /// than a count, since a large page made in a debug build under a
    /// loaded host's checks takes seconds.
    fn settle(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            app.poll(0);
            if app.loading.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the fetch did not answer");
    }

    /// Waits for the relayouts asked to come and takes them.
    fn relaid(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            app.poll(0);
            if app.current.relayout.is_none() && !app.current.again {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the relayout did not come");
    }

    /// The window resized to `width` and the page laid out again for it.
    fn resize(app: &mut App, width: usize) {
        app.input(Input::Resize(surface(width)), &mut Board::default());
        relaid(app);
    }

    /// Waits for every worker to end.
    fn idle(app: &App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
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

    /// The layout shown.
    fn laid(app: &App) -> &Laid {
        app.current.laid().unwrap()
    }

    /// The text between two places, as a copy takes it.
    fn between(laid: &Laid, start: Place, end: Place) -> String {
        let mut out = String::new();
        let _ = text::copy(laid, start, end, &mut |piece| {
            out.push_str(piece);
            ControlFlow::Continue(())
        });
        out
    }

    /// The whole page's text, as a copy takes it.
    fn page_text(app: &App) -> String {
        let laid = laid(app);
        let (start, end) = text::page_bounds(laid);
        between(laid, start, end)
    }

    /// The text line `line` of the layout shows.
    fn line_text(app: &App, line: usize) -> String {
        let laid = laid(app);
        let (start, end) = text::bounds(laid, &laid.lines()[line]).unwrap();
        between(laid, start, end)
    }

    /// The top's place.
    fn top(app: &App) -> Anchor {
        app.anchor().unwrap()
    }

    /// The line the top's place lies on.
    fn top_line(app: &App) -> usize {
        text::line_of(laid(app), top(app).place)
    }

    fn history(app: &App) -> (Vec<String>, Vec<String>) {
        let paths = |list: &Vec<Entry>| list.iter().map(|entry| entry.url.text()).collect();
        (paths(&app.back), paths(&app.forward))
    }

    /// `count` paragraphs, `line N` each.
    fn numbered(count: usize) -> String {
        (0..count).map(|n| format!("<p>line {n}</p>")).collect()
    }

    #[test]
    fn an_address_loads_and_titles_the_window() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        assert_eq!(app.title(), "Home - td-web");
        assert_eq!(app.address.text(), "https://example.org/");
        assert_eq!(laid(&app).links().len(), 2);
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
        idle(&app);
    }

    /// `Escape` stops a navigation and leaves the page shown as it was.
    #[test]
    fn escape_stops_a_navigation_and_leaves_the_page() {
        let (release, held) = mpsc::channel();
        let mut app = app_over(Arc::new(Held {
            table: site(),
            release: Mutex::new(held),
        }));
        type_address(&mut app, "example.org/a");
        release.send(()).unwrap();
        settle(&mut app);
        key(&mut app, "C-a");
        type_address(&mut app, "example.org/b");
        assert_eq!(app.status_text(), "loading https://example.org/b");
        assert_eq!(app.address.text(), "https://example.org/b");
        key(&mut app, "Escape");
        assert!(app.loading.is_none());
        assert_eq!(app.status_text(), "stopped");
        assert_eq!(app.address.text(), "https://example.org/a");
        assert_eq!(app.title(), "A - td-web");
        assert_eq!(page_text(&app), "page a");
        assert!(
            app.current.view.selection.is_some(),
            "a stop clears nothing else"
        );
        release.send(()).unwrap();
        idle(&app);
        app.poll(0);
        assert_eq!(app.title(), "A - td-web");
        assert_eq!(history(&app), (vec![], vec![]));
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

    /// Waits for the save out to answer; what the status row then says.
    fn save_done(app: &mut App) -> String {
        for _ in 0..1000 {
            app.poll(0);
            if app.saving.is_none() {
                return app.status_text().into_owned();
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the save did not answer");
    }

    fn downloads_site() -> Table {
        Table::default()
            .reply(
                "https://example.org/report",
                200,
                &[
                    ("content-type", "application/pdf"),
                    (
                        "content-disposition",
                        "attachment; filename=\"Q3 report.pdf\"",
                    ),
                ],
                b"%PDF-1.7 body",
            )
            .page("https://example.org/notes.txt", "text/plain", "some notes")
            .reply(
                "https://example.org/gone",
                404,
                &[("content-type", "application/zip")],
                b"PK",
            )
    }

    /// A home with an empty Downloads, as a host run's or a granted view.
    fn home() -> td_fs::TempDir {
        let temp = td_fs::TempDir::new("td-web-app-save").unwrap();
        std::fs::create_dir(temp.join("Downloads")).unwrap();
        temp
    }

    #[test]
    fn ctrl_s_saves_a_reply_not_shown_as_it_came_then_fetches_it_again() {
        let temp = home();
        let table = Arc::new(downloads_site());
        let mut app = app_over(table.clone());
        app.home = Some(temp.to_path_buf());
        typed(&mut app, "example.org/report");
        key(&mut app, "C-s");
        assert_eq!(app.status_text(), "saving in Downloads");
        assert_eq!(
            save_done(&mut app),
            "saved 13 bytes of example.org/report in Downloads as Q3 report.pdf"
        );
        let saved = temp.join("Downloads/Q3 report.pdf");
        assert_eq!(std::fs::read(saved).unwrap(), b"%PDF-1.7 body");
        assert_eq!(table.reach().len(), 1, "the body kept, not fetched again");
        // Given up once: the next save fetches it again, as a reload
        // would, and takes the next free name.
        key(&mut app, "C-s");
        assert_eq!(
            save_done(&mut app),
            "saved 13 bytes of example.org/report in Downloads as Q3 report (2).pdf"
        );
        assert_eq!(
            table.reach(),
            [
                ("https://example.org/report".to_string(), false),
                ("https://example.org/report".to_string(), false)
            ]
        );
        idle(&app);
        temp.cleanup().unwrap();
    }

    #[test]
    fn ctrl_s_saves_a_page_shown_as_its_server_sends_it_again() {
        let temp = home();
        let table = Arc::new(downloads_site());
        let mut app = app_over(table.clone());
        app.home = Some(temp.to_path_buf());
        typed(&mut app, "example.org/notes.txt");
        key(&mut app, "C-s");
        assert_eq!(
            save_done(&mut app),
            "saved 10 bytes of example.org/notes.txt in Downloads as notes.txt"
        );
        let saved = temp.join("Downloads/notes.txt");
        assert_eq!(std::fs::read(saved).unwrap(), b"some notes");
        assert_eq!(table.reach().len(), 2);
        idle(&app);
        temp.cleanup().unwrap();
    }

    #[test]
    fn nothing_is_saved_without_a_document_success_or_a_grant() {
        let temp = home();
        let table = Arc::new(downloads_site());
        let mut app = app_over(table.clone());
        app.home = Some(temp.to_path_buf());
        key(&mut app, "C-s");
        assert!(app.saving.is_none());
        assert!(
            app.status_text().starts_with("nothing to save"),
            "{}",
            app.status_text()
        );
        typed(&mut app, "example.org/gone");
        key(&mut app, "C-s");
        assert_eq!(app.status_text(), "not saved: the server answered 404");
        // In td-jail's home, Downloads is the human's only as a mounted
        // grant view, which this host has none of: nothing is fetched or
        // written, the body is kept, and the status row says how to grant.
        typed(&mut app, "example.org/report");
        app.home = Some(PathBuf::from(save::JAIL_HOME));
        if !Path::new("/home/td/Downloads").exists() {
            key(&mut app, "C-s");
            assert!(app.saving.is_none());
            let said = app.status_text();
            assert!(said.contains(save::GRANT), "{said}");
        }
        app.home = None;
        key(&mut app, "C-s");
        assert_eq!(
            app.status_text(),
            "not saved: HOME is not set to an absolute path"
        );
        app.home = Some(temp.to_path_buf());
        key(&mut app, "C-s");
        save_done(&mut app);
        assert_eq!(table.reach().len(), 2, "the two navigations alone");
        let names: Vec<_> = std::fs::read_dir(temp.join("Downloads"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["Q3 report.pdf"]);
        idle(&app);
        temp.cleanup().unwrap();
    }

    #[test]
    fn save_result_replaces_intervening_copy_feedback() {
        let temp = home();
        let (release, held) = mpsc::channel();
        let mut app = app_over(Arc::new(Held {
            table: downloads_site(),
            release: Mutex::new(held),
        }));
        app.home = Some(temp.to_path_buf());
        type_address(&mut app, "example.org/notes.txt");
        release.send(()).unwrap();
        settle(&mut app);
        key(&mut app, "C-s");
        key(&mut app, "C-c");
        assert_eq!(app.status_text(), "copied");
        release.send(()).unwrap();
        assert_eq!(
            save_done(&mut app),
            "saved 10 bytes of example.org/notes.txt in Downloads as notes.txt"
        );
        idle(&app);
        temp.cleanup().unwrap();
    }

    #[test]
    fn find_paints_copy_and_cut_refusals() {
        fn status_pixels(app: &mut App) -> Vec<u8> {
            let font = td_ui::font::pinned().unwrap();
            let surface = app.surface;
            let mut pixels = vec![0u8; surface.width * surface.height * 4];
            let mut raster = Raster::new(&mut pixels, &font, surface, surface.width * 4).unwrap();
            app.paint(&mut raster, surface).unwrap();
            let status = app.status().rect();
            let first = status.y as usize * surface.width * 4;
            let end = (status.y as usize + status.height as usize) * surface.width * 4;
            pixels[first..end].to_vec()
        }
        for chord in ["C-c", "C-x"] {
            let mut app = app(site());
            key(&mut app, "C-f");
            let query = "abcdefghij".repeat(10);
            app.query.set_text(&query).unwrap();
            app.query.act(Action::SelectAll).unwrap();
            td_alloc::with_reservation_limit(0, || key(&mut app, chord));
            assert_eq!(app.status_text(), "not copied: memory limit");
            assert_eq!(app.query.selected_text().unwrap(), Some(query.as_str()));
            let actual = status_pixels(&mut app);
            app.focus = Focus::Page;
            let expected = status_pixels(&mut app);
            assert!(
                actual == expected,
                "Find must paint the refusal for {chord}"
            );
            app.focus = Focus::Find;
            key(&mut app, "Left");
            assert!(app.message.is_none());
            assert!(
                status_pixels(&mut app) != expected,
                "next input restores Find"
            );
        }
    }

    #[test]
    fn one_save_at_a_time_and_the_status_row_says_so() {
        let temp = home();
        let (release, held) = mpsc::channel();
        let mut app = app_over(Arc::new(Held {
            table: downloads_site(),
            release: Mutex::new(held),
        }));
        app.home = Some(temp.to_path_buf());
        type_address(&mut app, "example.org/notes.txt");
        release.send(()).unwrap();
        settle(&mut app);
        key(&mut app, "C-s");
        app.poll(0);
        assert_eq!(app.status_text(), "saving in Downloads");
        assert_eq!(app.wait_ms(0), LOADING_WAIT_MS);
        key(&mut app, "C-s");
        assert_eq!(app.status_text(), "a save is under way");
        release.send(()).unwrap();
        assert_eq!(
            save_done(&mut app),
            "saved 10 bytes of example.org/notes.txt in Downloads as notes.txt"
        );
        idle(&app);
        temp.cleanup().unwrap();
    }

    /// The files a home's Downloads holds.
    fn downloaded(temp: &td_fs::TempDir) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = std::fs::read_dir(temp.join("Downloads"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    /// `Escape` stops a save still fetching at once, so a stalled server
    /// holds no later save back, and the stopped one writes nothing; its
    /// worker's slot is free once its fetch returns.
    #[test]
    fn escape_stops_a_save_still_fetching() {
        let temp = home();
        let (release, held) = mpsc::channel();
        let mut app = app_over(Arc::new(Held {
            table: downloads_site(),
            release: Mutex::new(held),
        }));
        app.home = Some(temp.to_path_buf());
        type_address(&mut app, "example.org/notes.txt");
        release.send(()).unwrap();
        settle(&mut app);
        key(&mut app, "C-s");
        assert!(app.saving.is_some());
        key(&mut app, "Escape");
        assert!(app.saving.is_none());
        assert_eq!(app.status_text(), "save stopped: nothing was written");
        // Another save starts at once, beside the stopped one's fetch.
        key(&mut app, "C-s");
        assert!(app.saving.is_some());
        assert_eq!(app.workers.load(Ordering::SeqCst), 2);
        release.send(()).unwrap();
        release.send(()).unwrap();
        assert_eq!(
            save_done(&mut app),
            "saved 10 bytes of example.org/notes.txt in Downloads as notes.txt"
        );
        idle(&app);
        assert_eq!(
            downloaded(&temp),
            ["notes.txt"],
            "the stopped one wrote nothing"
        );
        temp.cleanup().unwrap();
    }

    /// Closing the window drops a save still fetching and waits for one
    /// writing its file; a kept body writes from the start.
    #[test]
    fn closing_waits_for_a_write_not_for_a_fetch() {
        let temp = home();
        let (release, held) = mpsc::channel();
        let mut app = app_over(Arc::new(Held {
            table: downloads_site(),
            release: Mutex::new(held),
        }));
        app.home = Some(temp.to_path_buf());
        type_address(&mut app, "example.org/notes.txt");
        release.send(()).unwrap();
        settle(&mut app);
        key(&mut app, "C-s");
        app.finish_saving();
        assert!(app.saving.is_none());
        release.send(()).unwrap();
        idle(&app);
        assert!(downloaded(&temp).is_empty(), "dropped while it fetched");
        type_address(&mut app, "example.org/report");
        release.send(()).unwrap();
        settle(&mut app);
        key(&mut app, "C-s");
        app.finish_saving();
        assert_eq!(downloaded(&temp), ["Q3 report.pdf"], "written whole");
        assert_eq!(
            std::fs::read(temp.join("Downloads/Q3 report.pdf")).unwrap(),
            b"%PDF-1.7 body"
        );
        idle(&app);
        temp.cleanup().unwrap();
    }

    /// `Escape` does not stop a save writing its file: it clears the
    /// selection as it would with no save out, and the file is written.
    #[test]
    fn escape_leaves_a_write_to_finish_and_clears_as_ever() {
        let temp = home();
        let mut app = app(downloads_site());
        app.home = Some(temp.to_path_buf());
        typed(&mut app, "example.org/report");
        key(&mut app, "C-a");
        assert!(app.current.view.selection.is_some());
        // A kept body writes from the start.
        key(&mut app, "C-s");
        key(&mut app, "Escape");
        assert!(app.current.view.selection.is_none());
        assert_ne!(app.status_text(), "save stopped: nothing was written");
        assert_eq!(
            save_done(&mut app),
            "saved 13 bytes of example.org/report in Downloads as Q3 report.pdf"
        );
        assert_eq!(downloaded(&temp), ["Q3 report.pdf"]);
        idle(&app);
        temp.cleanup().unwrap();
    }

    /// A save that has answered but not yet been shown is shown by
    /// `Escape`, a failure as itself, not as stopped; the selection is
    /// cleared as ever.
    #[test]
    fn escape_shows_a_failed_save_rather_than_calling_it_stopped() {
        let temp = home();
        let (release, held) = mpsc::channel();
        let mut app = app_over(Arc::new(Held {
            table: downloads_site(),
            release: Mutex::new(held),
        }));
        app.home = Some(temp.to_path_buf());
        type_address(&mut app, "example.org/notes.txt");
        release.send(()).unwrap();
        settle(&mut app);
        key(&mut app, "C-a");
        key(&mut app, "C-s");
        // Downloads goes while the save fetches, so it fails to create.
        std::fs::remove_dir(temp.join("Downloads")).unwrap();
        release.send(()).unwrap();
        idle(&app);
        assert!(app.saving.is_some(), "answered, not yet shown");
        key(&mut app, "Escape");
        assert!(app.saving.is_none());
        let said = app.status_text();
        assert!(said.starts_with("not saved: "), "{said}");
        assert!(!said.contains("stopped"), "{said}");
        assert!(app.current.view.selection.is_none());
        temp.cleanup().unwrap();
    }

    #[test]
    fn a_page_that_could_not_be_fetched_is_not_saved() {
        let temp = home();
        let mut app = app(site());
        app.home = Some(temp.to_path_buf());
        typed(&mut app, "example.org/missing");
        key(&mut app, "C-s");
        assert!(app.saving.is_none());
        assert_eq!(
            app.status_text(),
            "not saved: the page could not be fetched"
        );
        temp.cleanup().unwrap();
    }

    #[test]
    fn a_failed_fetch_is_a_page_that_says_why() {
        let mut app = app(site());
        typed(&mut app, "example.org/missing");
        let text = page_text(&app);
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
        assert_eq!(page_text(&app), "there");
    }

    #[test]
    fn find_marks_matches_through_a_resize_and_copy_takes_the_focused_link() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        key(&mut app, "/");
        for c in ["t", "o"] {
            key(&mut app, c);
        }
        assert_eq!(app.current.view.hits().len(), 1);
        key(&mut app, "Escape");
        key(&mut app, "Tab");
        resize(&mut app, 400);
        assert_eq!(laid(&app).want, app.want());
        assert_eq!(app.current.view.hits().len(), 1, "kept");
        assert_eq!(app.current.view.focus, Some(0), "kept");
        let mut board = Board::default();
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
        assert_eq!(app.current.view.hits().len(), 1);
    }

    #[test]
    fn a_click_on_a_link_follows_it_and_the_inset_is_no_link() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        let link = point(&app, 0, 0);
        let page = app.page_rect();
        app.motion(Some((page.x + app.inset() - 1, link.1)));
        assert_eq!(app.hover, None, "the inset");
        app.motion(Some(link));
        assert_eq!(app.hover, Some(0));
        assert_eq!(app.status_text(), "https://example.org/a");
        app.mouse(PointerPhase::Press, link, false);
        assert!(app.loading.is_none(), "followed at the release");
        app.mouse(PointerPhase::Release, link, false);
        settle(&mut app);
        assert_eq!(app.title(), "A - td-web");
        assert_eq!(app.hover, None, "nothing under the pointer on the new page");
    }

    /// A navigated picture refused only because the page shown holds too
    /// much lets that page go, the pane showing paper, and is tried once
    /// more; Back still returns to the page. With nothing shown to let go
    /// it is refused at once (DESIGN.md §10, "Memory").
    #[test]
    fn a_picture_the_page_shown_crowds_out_lets_it_go_and_is_tried_once_more() {
        let words: String = (0..400).map(|i| format!("<p>words {i} more</p>")).collect();
        let table = || {
            site()
                .page("https://example.org/p", "text/html", &words)
                .reply(
                    "https://example.org/g.gif",
                    200,
                    &[("content-type", "image/gif")],
                    crate::picture::tests::GIF,
                )
        };
        let under = |limit: usize| {
            let root = Budget::new(limit).unwrap();
            let mut app = app(table());
            app.renderer = Renderer::with_budget(&root, None).unwrap();
            (app, root)
        };
        // The root's use alone and with the page shown, on a large root.
        let (mut probe, root) = under(memory::PAGES);
        let alone = root.used();
        typed(&mut probe, "example.org/p");
        let page = root.used() - alone;
        assert!(page > 64 << 10, "the page holds {page} bytes");
        drop(probe);

        // Room for the picture's work only once the page is let go.
        let (mut app, _root) = under(alone + memory::PICTURE_WORK + page / 2);
        typed(&mut app, "example.org/p");
        assert!(app.current.doc.page().is_some());
        typed(&mut app, "example.org/g.gif");
        assert!(
            matches!(app.current.doc.body, Body::Picture(_)),
            "shown after the page was let go"
        );
        key(&mut app, "M-Left");
        settle(&mut app);
        assert!(app.current.doc.page().is_some(), "Back returns to the page");

        // Nothing shown to let go: refused at once.
        let (mut app, _root) = under(alone + memory::PICTURE_WORK / 2);
        typed(&mut app, "example.org/g.gif");
        assert!(matches!(app.current.doc.body, Body::Resources));
    }

    /// With the lane full behind a relayout that runs on, a let-go is
    /// held, eight of them, and posted at the window's next poll, so it
    /// is still dropped on the render thread.
    #[test]
    fn a_let_go_the_full_lane_refuses_is_posted_at_the_next_poll() {
        use crate::document::tests::{again_of, hold, WAITS};
        let mut app = app(site());
        typed(&mut app, "example.org");
        let held = hold();
        let page = app.current.doc.page().unwrap();
        let want = Want {
            width: WAITS,
            ..app.want()
        };
        let again = again_of(page, want);
        let (faces, cancel, answer) = app.renderer.relayout_parts().unwrap();
        let posted = app.renderer.post(Lane::Again {
            again,
            faces,
            cancel,
            answer,
        });
        assert!(posted.is_ok());
        // The render thread takes it and waits; the lane fills behind it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut filled = 0;
        while app
            .renderer
            .post(Lane::Drop(Dropped::Document(Document::blank())))
            .is_ok()
        {
            filled += 1;
        }
        assert_eq!(filled, crate::worker::LANE);
        let (told, dropped) = mpsc::channel();
        for _ in 0..8 {
            let mut doc = Document::blank();
            doc.recorder = Some(told.clone());
            app.renderer.let_go(Dropped::Document(doc));
        }
        assert!(app.renderer.holding());
        drop(held);
        for _ in 0..2000 {
            app.poll(0);
            if !app.renderer.holding() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        for _ in 0..8 {
            let thread = dropped
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert_ne!(thread, std::thread::current().id());
        }
        assert!(!app.renderer.holding());
    }

    /// A relayout lets the old layout go first when the root has no room
    /// for both, and keeps it beside when it has.
    #[test]
    fn a_relayout_keeps_the_old_layout_only_where_both_fit() {
        let words: String = (0..400).map(|i| format!("<p>words {i} more</p>")).collect();
        let under = |limit: usize| {
            let root = Budget::new(limit).unwrap();
            let mut app = app(site().page("https://example.org/p", "text/html", &words));
            app.renderer = Renderer::with_budget(&root, None).unwrap();
            typed(&mut app, "example.org/p");
            (app, root)
        };
        let (mut app, root) = under(memory::PAGES);
        let used = root.used();
        app.input(Input::Resize(surface(500)), &mut Board::default());
        assert!(!app.current.relayout.as_ref().unwrap().let_go);
        assert!(app.current.laid().is_some(), "kept beside");
        relaid(&mut app);
        drop(app);
        let (mut app, _root) = under(used + (64 << 10));
        app.input(Input::Resize(surface(500)), &mut Board::default());
        assert!(app.current.relayout.as_ref().unwrap().let_go);
        assert!(app.current.laid().is_none(), "let go first");
        relaid(&mut app);
        assert!(app.current.laid().is_some());
    }

    /// Stopped, or overtaken, on the paper a page left for a picture's
    /// room: a stop shows that page again, and a navigation keeps it for
    /// Back. Its reload stopped, by a second stop or after a reload, the
    /// page is still held, for a reload and for Back.
    #[test]
    fn the_page_let_go_for_room_is_kept_through_a_stop_and_an_overtaking() {
        let words: String = (0..400).map(|i| format!("<p>words {i} more</p>")).collect();
        let table = || {
            site()
                .page("https://example.org/p", "text/html", &words)
                .reply(
                    "https://example.org/g.gif",
                    200,
                    &[("content-type", "image/gif")],
                    crate::picture::tests::GIF,
                )
        };
        let under = |limit: usize| {
            let root = Budget::new(limit).unwrap();
            let mut app = app(table());
            app.renderer = Renderer::with_budget(&root, None).unwrap();
            (app, root)
        };
        // What the render thread holds once all it was asked is done and
        // what it was given to let go is gone.
        let quiet = |app: &mut App| {
            relaid(app);
            app.renderer.idle();
            app.renderer.release();
            app.renderer.idle();
        };
        let (mut probe, root) = under(memory::PAGES);
        let alone = root.used();
        typed(&mut probe, "example.org/p");
        // Measured with nothing else held beside it, else the limit
        // below could leave the picture room without asking.
        quiet(&mut probe);
        let page = root.used() - alone;
        drop(probe);
        let room = |app: &mut App| {
            // The fetches before have ended, so this one is not refused.
            idle(app);
            type_address(app, "example.org/g.gif");
            // `settle`'s first half alone: the room made and answered, the
            // picture made again behind it never taken here, which a poll
            // could take in the same call and so pass this state by.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while std::time::Instant::now() < deadline {
                let asked = app
                    .loading
                    .as_ref()
                    .is_some_and(|loading| loading.room.try_recv().is_ok());
                if asked {
                    app.make_room();
                }
                app.answer_room();
                let answered = app.loading.as_ref().is_some_and(|l| l.owed.is_none());
                if app.current.held.is_some() && answered {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            panic!("no room asked");
        };
        // A stop: the page let go is shown again.
        let (mut app, _root) = under(alone + memory::PICTURE_WORK + page / 2);
        typed(&mut app, "example.org");
        typed(&mut app, "example.org/p");
        room(&mut app);
        key(&mut app, "Escape");
        settle(&mut app);
        assert_eq!(
            app.current.doc.url.as_ref().unwrap().text(),
            "https://example.org/p"
        );
        assert_eq!(history(&app).0, ["https://example.org/"]);
        // Overtaken: the page let go is kept for Back.
        let (mut app, _root) = under(alone + memory::PICTURE_WORK + page / 2);
        typed(&mut app, "example.org");
        typed(&mut app, "example.org/p");
        room(&mut app);
        typed(&mut app, "example.org/a");
        assert_eq!(
            history(&app).0,
            ["https://example.org/", "https://example.org/p"]
        );
        let held = |app: &App| app.current.held.as_ref().map(|e| e.url.text());
        for first in ["Escape", "C-r"] {
            let (mut app, _root) = under(alone + memory::PICTURE_WORK + page / 2);
            typed(&mut app, "example.org");
            typed(&mut app, "example.org/p");
            room(&mut app);
            // The reload stopped before it arrives.
            key(&mut app, first);
            assert!(app.loading.as_ref().is_some_and(|l| l.restores));
            key(&mut app, "Escape");
            assert!(app.loading.is_none(), "{first}");
            assert_eq!(app.status_text(), "stopped");
            assert_eq!(held(&app).as_deref(), Some("https://example.org/p"));
            key(&mut app, "C-r");
            settle(&mut app);
            assert_eq!(
                app.current.doc.url.as_ref().unwrap().text(),
                "https://example.org/p"
            );
            assert_eq!(history(&app).0, ["https://example.org/"]);
        }
        // A reload refused before it starts holds the page still.
        let (mut app, _root) = under(alone + memory::PICTURE_WORK + page / 2);
        typed(&mut app, "example.org");
        typed(&mut app, "example.org/p");
        room(&mut app);
        app.workers.fetch_add(MAX_WORKERS, Ordering::SeqCst);
        key(&mut app, "Escape");
        app.workers.fetch_sub(MAX_WORKERS, Ordering::SeqCst);
        assert!(app.loading.is_none());
        assert_eq!(held(&app).as_deref(), Some("https://example.org/p"));
    }

    /// A picture's ask for room is answered only once what was let go
    /// for it has left the window: a let-go the full lane holds is
    /// posted first. A render thread lost and not started again answers
    /// it refused rather than leaving the fetch waiting.
    #[test]
    fn room_is_answered_once_the_page_let_go_has_left_the_window() {
        use crate::document::tests::{again_of, hold, WAITS};
        let asking = |app: &mut App| {
            let (released, given) = mpsc::channel();
            let (_ask, room) = mpsc::channel();
            app.loading = Some(Loading {
                url: Url::parse("https://example.org/g.gif").unwrap(),
                fragment: None,
                motion: Motion::New,
                top: None,
                answer: None,
                stopped: Stopped(Arc::new(AtomicBool::new(false))),
                room,
                released,
                owed: None,
                restores: false,
                styles: mpsc::channel().1,
                styling: false,
            });
            given
        };
        let mut app = app(site());
        typed(&mut app, "example.org");
        let held = hold();
        let want = Want {
            width: WAITS,
            ..app.want()
        };
        let again = again_of(app.current.doc.page().unwrap(), want);
        let (faces, cancel, answer) = app.renderer.relayout_parts().unwrap();
        let posted = app.renderer.post(Lane::Again {
            again,
            faces,
            cancel,
            answer,
        });
        assert!(posted.is_ok());
        std::thread::sleep(std::time::Duration::from_millis(50));
        while app
            .renderer
            .post(Lane::Drop(Dropped::Document(Document::blank())))
            .is_ok()
        {}
        let given = asking(&mut app);
        app.make_room();
        assert!(app.renderer.holding());
        assert!(given.try_recv().is_err(), "not while the page is held");
        app.poll(0);
        assert!(given.try_recv().is_err());
        drop(held);
        let answered = (0..2000).find_map(|_| {
            app.poll(0);
            std::thread::sleep(std::time::Duration::from_millis(2));
            given.try_recv().ok()
        });
        assert_eq!(answered, Some(true));
        assert!(!app.renderer.holding());

        // Lost, with no room to start it again.
        let root = Budget::new(memory::PAGES).unwrap();
        let mut app = self::app(site());
        app.renderer = Renderer::with_budget(&root, None).unwrap();
        typed(&mut app, "example.org");
        app.renderer.stop_thread();
        let _full = root.charge(root.limit() - root.used()).unwrap();
        let given = asking(&mut app);
        app.make_room();
        assert!(app.renderer.holding());
        assert!(app.renderer.lost());
        assert_eq!(given.try_recv(), Ok(false));
    }

    /// The pointer over an image in no link names where a click opens it.
    #[test]
    fn an_unlinked_image_under_the_pointer_names_its_src() {
        let page = "<p><img src=/pic alt=picture>";
        let mut app = app(site().page("https://example.org/i", "text/html", page));
        typed(&mut app, "example.org/i");
        app.motion(Some(point(&app, 0, 1)));
        assert_eq!(app.status_text(), "https://example.org/pic");
    }

    /// The page let go at a navigation is dropped on the render thread,
    /// as is a relayout's answer it had not taken (DESIGN.md §10,
    /// "Pipeline and threads").
    #[test]
    fn the_page_let_go_is_dropped_on_the_render_thread() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        let (told, dropped) = mpsc::channel();
        app.current.doc.recorder = Some(told);
        // A relayout asked and answered, not yet taken.
        resize(&mut app, 600);
        app.input(Input::Resize(surface(500)), &mut Board::default());
        idle(&app);
        typed(&mut app, "example.org/a");
        idle(&app);
        let thread = dropped.recv().unwrap();
        assert_ne!(thread, std::thread::current().id());
        // A navigation stopped with its answer in hand: the answer is let
        // go there too.
        let mut app2 = self::app(site());
        type_address(&mut app2, "example.org/a");
        let loading = app2.loading.as_ref().unwrap();
        let answer = loading.answer.as_ref().unwrap();
        let arrived = answer.recv_timeout(std::time::Duration::from_secs(5));
        let Ok(Some((mut doc, fragment))) = arrived else {
            panic!("no answer");
        };
        let (told, dropped) = mpsc::channel();
        doc.recorder = Some(told);
        // Put back, as if not yet taken, then stopped.
        let (send, receive) = mpsc::channel();
        send.send(Some((doc, fragment))).unwrap();
        app2.loading.as_mut().unwrap().answer = Some(receive);
        key(&mut app2, "Escape");
        assert!(app2.loading.is_none());
        idle(&app2);
        assert_ne!(dropped.recv().unwrap(), std::thread::current().id());
    }

    /// A relayout's new page replaces the page shown, which, with the
    /// layout it held, is dropped on the render thread, not the window's.
    #[test]
    fn a_replaced_page_and_its_layout_are_dropped_on_the_render_thread() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        let (told, dropped) = mpsc::channel();
        app.current.doc.page_mut().unwrap().recorder = Some(crate::pipeline::Recorder(told));
        resize(&mut app, 500);
        idle(&app);
        let thread = dropped
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_ne!(thread, std::thread::current().id());
    }

    /// A history entry made while a relayout that let the old layout go
    /// runs keeps the top the relayout was asked with.
    #[test]
    fn an_entry_made_during_a_let_go_relayout_keeps_its_top() {
        let mut app = app(site().page("https://example.org/n", "text/html", &numbered(400)));
        typed(&mut app, "example.org/n");
        for _ in 0..3 {
            key(&mut app, "PageDown");
        }
        let anchor = top(&app);
        let laid = app.current.doc.page_mut().unwrap().laid.take();
        let (_, cancel, answer) = app.renderer.relayout_parts().unwrap();
        app.current.relayout = Some(Relayout {
            answer,
            cancel,
            top: Some(anchor),
            focused: None,
            want: app.want(),
            let_go: true,
        });
        let entry = app.entry().unwrap();
        assert_eq!(entry.top.map(|top| top.place), Some(anchor.place));
        app.current.relayout = None;
        drop(laid);
    }

    /// Scrolling while a relayout runs is kept: the top is the one shown
    /// when the relayout comes, not when it was asked.
    #[test]
    fn scrolling_while_a_relayout_runs_is_kept() {
        let mut app = app(site().page("https://example.org/n", "text/html", &numbered(400)));
        typed(&mut app, "example.org/n");
        for _ in 0..3 {
            key(&mut app, "PageDown");
        }
        app.input(Input::Resize(surface(700)), &mut Board::default());
        for _ in 0..5 {
            key(&mut app, "PageDown");
        }
        let before = top(&app);
        relaid(&mut app);
        assert_eq!(top(&app).place, before.place);
    }

    /// A step down lays another tree: places of the old one are let go,
    /// the selection, matches and focus with them.
    #[test]
    fn a_step_down_lets_the_old_trees_places_go() {
        let mut app = app(site().page(
            "https://example.org/s",
            "text/html",
            "<p>alpha <a href=/a>needle</a> beta",
        ));
        typed(&mut app, "example.org/s");
        key(&mut app, "C-a");
        key(&mut app, "Tab");
        key(&mut app, "C-a");
        assert!(app.current.view.selection.is_some());
        // Another tree's page, answered as a relayout.
        let going = Arc::new(AtomicBool::new(false));
        let other = app
            .renderer
            .document(
                Url::parse("https://example.org/s").unwrap(),
                200,
                Some("text/html"),
                b"<p>alpha needle beta".to_vec(),
                app.want(),
                &going,
            )
            .unwrap();
        let Body::Page(page) = std::mem::replace(&mut { other }.body, Body::Blank) else {
            panic!("a page");
        };
        let (_, cancel, answer) = app.renderer.relayout_parts().unwrap();
        *answer.lock().unwrap() = Some(Ok(page));
        app.current.relayout = Some(Relayout {
            answer,
            cancel,
            top: app.anchor(),
            focused: None,
            want: app.want(),
            let_go: false,
        });
        app.poll(0);
        assert!(app.current.relayout.is_none());
        assert_eq!(app.current.view.selection, None);
        assert_eq!(app.current.view.focus, None);
        assert!(app.current.view.hits.is_none());
    }

    /// A text page ending in a newline, its last line empty, copies its
    /// text, not the page's address.
    #[test]
    fn a_text_page_ending_in_a_newline_copies_its_text() {
        let mut app = app(site().page("https://example.org/ip", "text/plain", "203.0.113.7\n"));
        typed(&mut app, "example.org/ip");
        key(&mut app, "C-a");
        assert_eq!(copied(&mut app), ["203.0.113.7"]);
    }

    /// A link whose href does not resolve is shown and copied as written.
    #[test]
    fn a_link_that_does_not_resolve_is_shown_as_written() {
        let mut app = app(site().page(
            "https://example.org/u",
            "text/html",
            "<p><a href=\"http://[bad\">odd</a>",
        ));
        typed(&mut app, "example.org/u");
        key(&mut app, "Tab");
        assert_eq!(app.status_text(), "http://[bad");
    }

    /// A page arriving under a still pointer says what is under it.
    #[test]
    fn a_page_arriving_under_a_still_pointer_is_hovered() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        let link = point(&app, 0, 0);
        app.motion(Some(link));
        assert_eq!(app.hover, Some(0));
        typed(&mut app, "example.org/a");
        app.pointer = Some(link);
        assert_eq!(app.hover, None);
        key(&mut app, "M-Left");
        settle(&mut app);
        assert_eq!(app.hover, Some(0), "hovered without a motion");
    }

    /// A lane relayout whose job ends without answering, as a panic
    /// does, is answered refused; a post to a lost render thread starts
    /// another.
    #[test]
    fn a_lost_relayout_is_answered_and_a_lost_thread_started_again() {
        let renderer = Renderer::start(None).unwrap();
        let (_, _, answer) = renderer.relayout_parts().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _unanswered = document::tests::unanswered(&answer);
            panic!("the job");
        }));
        assert!(result.is_err());
        assert!(matches!(
            answer.lock().unwrap().take(),
            Some(Err(Refusal::Limit(_)))
        ));
        renderer.stop_thread();
        let (told, dropped) = mpsc::channel();
        let mut doc = Document::blank();
        doc.recorder = Some(told);
        renderer.let_go(Dropped::Document(doc));
        let thread = dropped
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_ne!(thread, std::thread::current().id());
    }

    /// A click on an image with no link opens its `src`; one in a link
    /// follows the link (DESIGN.md §10, "Links").
    #[test]
    fn a_click_on_an_unlinked_image_opens_its_src() {
        let page = "<p><img src=/a alt=picture> <a href=/b><img src=/c alt=linked></a>";
        let mut app = app(site().page("https://example.org/i", "text/html", page));
        typed(&mut app, "example.org/i");
        let image = point(&app, 0, 1);
        app.mouse(PointerPhase::Press, image, false);
        app.mouse(PointerPhase::Release, image, false);
        settle(&mut app);
        assert_eq!(app.title(), "A - td-web");
        key(&mut app, "M-Left");
        settle(&mut app);
        let linked = point(&app, 0, 9);
        app.mouse(PointerPhase::Press, linked, false);
        app.mouse(PointerPhase::Release, linked, false);
        settle(&mut app);
        assert_eq!(app.title(), "B - td-web");
    }

    /// A link on an unwrapped line past the pane's edge is laid whole:
    /// it takes focus there, the page scrolled across to show it, and
    /// the status row names where it goes.
    #[test]
    fn a_link_past_an_unwrapped_lines_edge_takes_focus_and_is_named() {
        let page = format!(
            "<pre>{}<a href=\"/far\">far</a></pre><p><a href=\"/near\">near</a>",
            "x".repeat(300)
        );
        let mut app = app(site().page("https://example.org/w", "text/html", &page));
        typed(&mut app, "example.org/w");
        assert_eq!(line_text(&app, 0), format!("{}far", "x".repeat(300)));
        key(&mut app, "Tab");
        assert_eq!(app.current.view.focus, Some(0));
        assert_eq!(app.status_text(), "https://example.org/far");
        assert!(app.current.view.x > 0, "scrolled across to it");
        key(&mut app, "Tab");
        assert_eq!(app.status_text(), "https://example.org/near");
        assert_eq!(app.current.view.x, 0);
    }

    /// The status row says where the reader is by the layout's lines: the
    /// last one shown whole, of all of them; and nothing on a page shown
    /// whole.
    #[test]
    fn the_status_row_counts_the_layouts_lines() {
        let lines: String = (0..200)
            .map(|n| format!("<p>line {n} with words enough to wrap twice here</p>"))
            .collect();
        let mut app = app(site().page("https://example.org/long", "text/html", &lines));
        typed(&mut app, "example.org/long");
        assert_eq!(app.status_text(), "17 of 200 lines");
        resize(&mut app, 200);
        let count = laid(&app).lines().len();
        assert!(count > 400, "lines wrapped: {count}");
        let (bottom, _) = app.current.view.bottom(laid(&app), &app.pane());
        assert!(bottom > 0);
        assert_eq!(app.status_text(), format!("{bottom} of {count} lines"));
        key(&mut app, "End");
        assert_eq!(app.status_text(), format!("{count} of {count} lines"));
        typed(&mut app, "example.org/a");
        assert_eq!(app.status_text(), "");
    }

    #[test]
    fn hover_follows_the_page_under_a_still_pointer() {
        let lines = format!("<p><a href=\"/a\">to a</a></p>{}", numbered(200));
        let mut app = app(site().page("https://example.org/long", "text/html", &lines));
        typed(&mut app, "example.org/long");
        app.motion(Some(point(&app, 0, 1)));
        assert_eq!(app.hover, Some(0));
        for _ in 0..3 {
            key(&mut app, "Down");
        }
        assert_eq!(app.hover, None, "the link scrolled away");
        for _ in 0..3 {
            key(&mut app, "Up");
        }
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

    /// The wheel's columns scroll a page wider than the pane across, as
    /// `Left` and `Right` do.
    #[test]
    fn the_wheels_columns_scroll_across() {
        let page = format!("<pre>{}</pre>", "x".repeat(300));
        let mut app = app(site().page("https://example.org/w", "text/html", &page));
        typed(&mut app, "example.org/w");
        let mut board = Board::default();
        app.input(
            Input::Wheel {
                rows: 0,
                columns: 2,
            },
            &mut board,
        );
        let across = app.current.view.x;
        assert!(across > 0);
        key(&mut app, "Left");
        key(&mut app, "Left");
        assert_eq!(app.current.view.x, 0);
        key(&mut app, "Right");
        app.input(
            Input::Wheel {
                rows: 0,
                columns: 1,
            },
            &mut board,
        );
        assert_eq!(app.current.view.x, across);
        app.input(
            Input::Wheel {
                rows: 0,
                columns: -9,
            },
            &mut board,
        );
        assert_eq!(app.current.view.x, 0);
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

    /// A URL's fragment scrolls to the element whose `id`, else whose
    /// `a`'s `name`, it names, else to the top; one on the page shown
    /// scrolls without a fetch.
    #[test]
    fn a_url_fragment_scrolls_to_its_element() {
        let page = format!(
            "<p><a href=\"#t\">to t</a> <a href=\"#n\">to n</a></p>{}<p id=t>target</p>{}\
             <p><a name=n>named</a></p>{}",
            numbered(50),
            numbered(50),
            numbered(50)
        );
        let table = Arc::new(site().page("https://example.org/f", "text/html", &page));
        let mut app = app_over(table.clone());
        let shown_at = |app: &App, text: &str| {
            let laid = laid(app);
            let at = (0..laid.lines().len())
                .find(|&at| line_text(app, at) == text)
                .unwrap();
            laid.lines()[at].y as i64
        };
        typed(&mut app, "example.org/f#t");
        assert_eq!(app.address.text(), "https://example.org/f#t");
        assert_eq!(app.current.view.y, shown_at(&app, "target"));
        assert_eq!(line_text(&app, top_line(&app)), "target");
        key(&mut app, "Home");
        key(&mut app, "Tab");
        key(&mut app, "Tab");
        key(&mut app, "Return");
        assert!(app.loading.is_none(), "no fetch for the page shown");
        assert_eq!(table.reach().len(), 1);
        assert_eq!(app.address.text(), "https://example.org/f#n");
        assert_eq!(app.current.view.y, shown_at(&app, "named"));
        typed(&mut app, "example.org/f#nothing");
        assert_eq!(app.current.view.y, 0, "a fragment naming nothing");
    }

    #[test]
    fn f8_sets_body_text_in_serif_and_back_and_is_listed() {
        let mut app = app(site());
        typed(&mut app, "example.org");
        assert_eq!(laid(&app).want.body, TextFamily::Sans);
        key(&mut app, "F8");
        assert!(
            app.status_text().contains("Serif") && app.status_text().contains("not loaded"),
            "{}",
            app.status_text()
        );
        relaid(&mut app);
        assert_eq!(laid(&app).want.body, TextFamily::Serif);
        assert_eq!(laid(&app).want, app.want());
        key(&mut app, "F8");
        relaid(&mut app);
        assert_eq!(laid(&app).want.body, TextFamily::Sans);
        assert!(BROWSER_KEYS.iter().any(|(keys, _)| *keys == "F8"));
    }

    /// `F8` lays the page out again, its styles kept, and keeps the
    /// selection and the top's place.
    #[test]
    fn f8_lays_out_again_and_keeps_the_selection() {
        let mut app = one_paragraph();
        // The line after the top's, which is in part above the pane.
        let line = top_line(&app) + 1;
        app.drag_from(point(&app, line, 0), point(&app, line + 2, 3));
        let selection = app.current.view.selection;
        let copy = copied(&mut app);
        assert!(copy[0].len() > 100, "{copy:?}");
        let styled = laid(&app).styled_ref().try_clone().unwrap();
        let place = top(&app).place;
        key(&mut app, "F8");
        assert!(app.current.relayout.is_some() || app.current.again);
        relaid(&mut app);
        assert_eq!(laid(&app).want.body, TextFamily::Serif);
        assert!(
            BudgetShared::ptr_eq(laid(&app).styled_ref(), &styled),
            "a layout only"
        );
        assert_eq!(app.current.view.selection, selection);
        assert_eq!(copied(&mut app), copy);
        assert_eq!(top(&app).place, place);
    }

    /// `F9` shows the page without its own styles, and with them again;
    /// on a page with none of its own it says so and lays nothing.
    #[test]
    fn f9_toggles_the_pages_own_styles() {
        let table = site()
            .page(
                "https://example.org/s",
                "text/html",
                "<style>p{display:none}</style><p>hidden text</p>",
            )
            .page("https://example.org/t.txt", "text/plain", "plain text");
        let mut app = app(table);
        key(&mut app, "F9");
        assert_eq!(app.status_text(), "F9 shows a page without its styles");
        typed(&mut app, "example.org/s");
        assert_eq!(page_text(&app), "");
        key(&mut app, "F9");
        assert_eq!(app.status_text(), "the page without its own styles");
        relaid(&mut app);
        assert!(!laid(&app).want.styles);
        assert_eq!(page_text(&app), "hidden text");
        app.message = None;
        assert!(
            app.status_text().contains("without the page's styles (F9)"),
            "{}",
            app.status_text()
        );
        key(&mut app, "F9");
        assert_eq!(app.status_text(), "the page's own styles");
        relaid(&mut app);
        assert_eq!(page_text(&app), "");
        app.message = None;
        assert!(!app.status_text().contains("F9"), "{}", app.status_text());
        // Off again, then another page: it comes with its styles.
        key(&mut app, "F9");
        relaid(&mut app);
        typed(&mut app, "example.org/t.txt");
        assert!(app.current.styles);
        assert!(laid(&app).want.styles);
        key(&mut app, "F9");
        assert_eq!(app.status_text(), "this page shows no styles of its own");
        assert!(app.current.styles);
        assert!(app.current.relayout.is_none() && !app.current.again);
        assert!(BROWSER_KEYS.iter().any(|(keys, _)| *keys == "F9"));
    }

    /// A page that is one long paragraph of numbered words, and an app
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

    /// The first word of the line at the top.
    fn first_word(app: &App) -> String {
        let text = line_text(app, top_line(app));
        text.split(' ').next().unwrap().to_string()
    }

    #[test]
    fn a_resize_or_f8_keeps_the_place_inside_a_logical_line() {
        let mut app = one_paragraph();
        assert_eq!(
            text::paragraph(laid(&app), 0),
            (0, laid(&app).lines().len() - 1)
        );
        // Each time the line at the top holds the word the last top
        // began with.
        for change in [Some(300), Some(800), None] {
            let word = first_word(&app);
            match change {
                Some(width) => resize(&mut app, width),
                None => {
                    key(&mut app, "F8");
                    relaid(&mut app);
                }
            }
            assert!(top_line(&app) > 0, "not the paragraph's start");
            let shown = line_text(&app, top_line(&app));
            assert!(shown.split(' ').any(|w| w == word), "{word} in {shown}");
        }
    }

    /// A resize asks the render thread for a layout at the new width; the
    /// old one is shown meanwhile, the status row saying so, and the new
    /// one shows the line that held the top's place at the top.
    #[test]
    fn a_resize_lays_the_page_out_again_and_keeps_the_top() {
        let mut app = one_paragraph();
        let before = top(&app);
        let word = first_word(&app);
        app.input(Input::Resize(surface(300)), &mut Board::default());
        assert!(app.current.relayout.is_some());
        assert_eq!(laid(&app).want.width, 784.0, "the old layout shown");
        assert!(
            app.status_text().contains("laying out"),
            "{}",
            app.status_text()
        );
        assert_eq!(app.wait_ms(0), LOADING_WAIT_MS);
        relaid(&mut app);
        assert_eq!(laid(&app).want.width, 284.0);
        assert!(!app.status_text().contains("laying out"));
        let after = top(&app);
        assert_eq!(
            text::line_of(laid(&app), before.place),
            text::line_of(laid(&app), after.place)
        );
        let shown = line_text(&app, top_line(&app));
        assert!(shown.split(' ').any(|w| w == word), "{word} in {shown}");
        // Resized and back before the first comes: the page is laid for
        // the window's width at the end.
        app.input(Input::Resize(surface(500)), &mut Board::default());
        app.input(Input::Resize(surface(300)), &mut Board::default());
        relaid(&mut app);
        assert_eq!(laid(&app).want, app.want());
        app.input(Input::Resize(surface(500)), &mut Board::default());
        app.input(Input::Resize(surface(600)), &mut Board::default());
        relaid(&mut app);
        assert_eq!(laid(&app).want, app.want());
    }

    #[test]
    fn back_after_a_resize_opens_at_the_same_text() {
        let mut app = one_paragraph();
        resize(&mut app, 300);
        let word = first_word(&app);
        typed(&mut app, "example.org/a");
        resize(&mut app, 800);
        key(&mut app, "M-Left");
        settle(&mut app);
        relaid(&mut app);
        assert!(top_line(&app) > 0);
        let shown = line_text(&app, top_line(&app));
        assert!(shown.split(' ').any(|w| w == word), "{word} in {shown}");
    }

    /// A history step back opens the page at the top's place and offset
    /// it had.
    #[test]
    fn a_step_back_restores_the_top() {
        let mut app = app(site().page("https://example.org/long", "text/html", &numbered(200)));
        typed(&mut app, "example.org/long");
        for _ in 0..10 {
            key(&mut app, "Down");
        }
        let (y, anchor) = (app.current.view.y, top(&app));
        assert_eq!(y, 160);
        typed(&mut app, "example.org/a");
        key(&mut app, "M-Left");
        settle(&mut app);
        assert_eq!(app.current.view.y, y);
        assert_eq!(top(&app), anchor);
        key(&mut app, "M-Right");
        settle(&mut app);
        assert_eq!(app.current.view.y, 0);
    }

    #[test]
    fn a_relayout_marks_matches_again_without_moving_to_one() {
        let mut app = app(site().page("https://example.org/long", "text/html", &numbered(200)));
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
        assert!(app.current.view.y > 0, "find moved to a match");
        let at = app.current.view.at;
        let hits = app.current.view.hits().len();
        assert_eq!(hits, 11);
        key(&mut app, "Escape");
        key(&mut app, "Home");
        let selection = app.current.view.selection;
        resize(&mut app, 400);
        assert_eq!(app.current.view.y, 0, "not moved to the match");
        assert_eq!(app.current.view.hits().len(), hits);
        assert_eq!(app.current.view.at, at);
        assert_eq!(app.current.view.selection, selection);
    }

    #[test]
    fn a_stopped_or_overtaken_navigation_tells_its_worker() {
        let (release, held) = mpsc::channel();
        let mut app = app_over(Arc::new(Held {
            table: site(),
            release: Mutex::new(held),
        }));
        type_address(&mut app, "example.org/a");
        let first = Arc::clone(&app.loading.as_ref().unwrap().stopped.0);
        type_address(&mut app, "example.org/b");
        assert!(first.load(Ordering::SeqCst), "overtaken");
        let second = Arc::clone(&app.loading.as_ref().unwrap().stopped.0);
        assert!(!second.load(Ordering::SeqCst));
        key(&mut app, "Escape");
        assert!(second.load(Ordering::SeqCst), "stopped");
        release.send(()).unwrap();
        release.send(()).unwrap();
        idle(&app);
    }

    #[test]
    fn a_narrow_pane_lays_at_the_least_width_and_cuts_lines_at_its_edge() {
        let mut app = one_paragraph();
        resize(&mut app, 40);
        let least = MIN_WRAP_EMS * i64::from(td_ui::prose::SIZE);
        assert_eq!(laid(&app).want.width, least as f64);
        let pane = app.pane();
        let edge = pane.rect.x + i64::from(pane.rect.width);
        let left = pane.rect.x + pane.inset;
        assert!(
            laid(&app)
                .lines()
                .iter()
                .any(|line| left + text::horizontal(line).1 as i64 > edge),
            "lines wider than the pane"
        );
        let pens: Vec<i64> = painted(&mut app).iter().map(|&(x, _, _)| x).collect();
        assert!(!pens.is_empty());
        let overhang = i64::from(td_ui::prose::OVERHANG);
        assert!(pens.iter().all(|&x| x < edge + overhang), "{pens:?}");
    }

    /// `Left` and `Right` scroll a page wider than the pane across it,
    /// within its extent; a page that fits does not move.
    #[test]
    fn left_and_right_scroll_a_page_wider_than_the_pane() {
        let page = format!("<pre>{}END</pre>", "x".repeat(300));
        let mut app = app(site().page("https://example.org/w", "text/html", &page));
        typed(&mut app, "example.org/w");
        key(&mut app, "Right");
        assert_eq!(app.current.view.x, 4 * CELL_WIDTH as i64);
        key(&mut app, "Left");
        assert_eq!(app.current.view.x, 0);
        key(&mut app, "Left");
        assert_eq!(app.current.view.x, 0);
        for _ in 0..200 {
            key(&mut app, "Right");
        }
        let most = app.current.view.x;
        key(&mut app, "Right");
        assert_eq!(app.current.view.x, most, "the extent's edge");
        let pane = app.pane();
        let right = text::horizontal(&laid(&app).lines()[0]).1 as i64;
        assert!(most + pane.width() >= right, "{most} {right}");
        assert!(most < right);
        // Its end is now on the screen.
        let end = point(&app, 0, 300);
        assert!(pane.rect.contains(end.0, end.1));
        let mut app2 = app_over(Arc::new(site()));
        typed(&mut app2, "example.org");
        key(&mut app2, "Right");
        assert_eq!(app2.current.view.x, 0);
    }

    #[test]
    fn a_resize_keeps_the_line_at_the_top() {
        let page: String = (0..100)
            .map(|n| format!("<p>paragraph {n} has words enough to wrap</p>"))
            .collect();
        let mut app = app(site().page("https://example.org/p", "text/html", &page));
        typed(&mut app, "example.org/p");
        while top_line(&app) < 41 {
            key(&mut app, "Down");
        }
        assert_eq!(top_line(&app), 41);
        assert_eq!(line_text(&app, 41), "paragraph 41 has words enough to wrap");
        let place = top(&app).place;
        let wide = laid(&app).lines().len();
        resize(&mut app, 200);
        assert!(laid(&app).lines().len() > wide);
        let at = top_line(&app);
        assert_eq!(top(&app).place, place);
        assert_eq!(text::paragraph(laid(&app), at).0, at, "its first line");
        assert!(line_text(&app, at).starts_with("paragraph 41 "));
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

    /// A point a pixel into cell `glyph` of line `line` of the layout:
    /// without faces every scalar is a cell, so its left half.
    fn point(app: &App, line: usize, glyph: usize) -> (i64, i64) {
        let pane = app.pane();
        let frame = app.current.view.frame(&pane);
        let line = &laid(app).lines()[line];
        let left = text::horizontal(line).0;
        let scale = pane.scale as f64;
        let x = (left + (CELL_WIDTH * glyph) as f64) * scale;
        (
            frame.x + x as i64 + 1,
            frame.y + (line.y * scale) as i64 + 1,
        )
    }

    /// Each glyph a paint of the page draws: its pen, its scalar and
    /// whether it is on the selection's ground.
    fn painted(app: &mut App) -> Vec<(i64, char, bool)> {
        let pane = app.pane();
        let frame = app.current.view.frame(&pane);
        let view = &app.current.view;
        let marks = Marks {
            hits: view.hits(),
            selection: view.selection.and_then(Selection::range),
            link: None,
        };
        let mut scratch = Scratch::new(app.renderer.budget());
        let (laid, measure) =
            measure_of(&app.current.doc, &mut app.current.measure, &app.renderer).unwrap();
        let mut glyphs = Vec::new();
        paint::paint(
            &laid,
            measure,
            &frame,
            app.surface.bounds(),
            &marks,
            &mut scratch,
            &mut |draw| {
                if let td_ui::raster::Primitive::Glyph {
                    x, scalar, style, ..
                } = draw.primitive
                {
                    glyphs.push((x, scalar, style.background == td_ui::raster::SELECTED));
                }
            },
        );
        glyphs
    }

    /// The glyphs the page paints on the selection's ground.
    fn painted_selection(app: &mut App) -> String {
        painted(app)
            .into_iter()
            .filter(|&(_, _, selected)| selected)
            .map(|(_, scalar, _)| scalar)
            .collect()
    }

    /// Two paragraphs on a pane 240 pixels wide, the first wrapped.
    fn two_paragraphs() -> App {
        let page = "<p>alpha beta gamma delta epsilon zeta</p><p>second para</p>";
        let mut app = app(site().page("https://example.org/t", "text/html", page));
        resize(&mut app, 240);
        typed(&mut app, "example.org/t");
        let lines: Vec<String> = (0..laid(&app).lines().len())
            .map(|at| line_text(&app, at))
            .collect();
        assert_eq!(
            lines,
            ["alpha beta gamma delta", "epsilon zeta", "second para"]
        );
        app
    }

    #[test]
    fn copying_refuses_every_reservation_without_replacing_the_clipboard_or_selection() {
        let mut app = app(site().page(
            "https://example.org/copy",
            "text/plain",
            &"second para\n".repeat(100),
        ));
        typed(&mut app, "example.org/copy");
        key(&mut app, "C-a");
        let before = (
            app.current.view.selection,
            app.current.view.y,
            app.current.view.focus,
        );
        let memory = app.renderer.budget().try_clone().unwrap();
        let used = memory.used();
        // The reservations a copy takes.
        const NEEDED: usize = 3;
        for allowed in 0..=NEEDED {
            let mut board = Board {
                copies: vec!["previous".into()],
            };
            td_alloc::with_reservation_limit(allowed, || app.copy(&mut board));
            assert_eq!(
                (
                    app.current.view.selection,
                    app.current.view.y,
                    app.current.view.focus
                ),
                before
            );
            assert_eq!(memory.used(), used);
            if allowed < NEEDED {
                assert_eq!(board.copies, ["previous"], "{allowed}");
                assert_eq!(app.message.as_deref(), Some("not copied: memory limit"));
            } else {
                assert_eq!(board.copies.len(), 2);
                assert!(board.copies[1].contains("second para"));
                assert_eq!(app.message.as_deref(), Some("copied"));
            }
        }
        let pressure = memory.charge(memory.limit() - memory.used()).unwrap();
        assert!(matches!(
            app.selected_text(),
            Some(Err(TextError::Resource(td_alloc::BudgetError::Limit)))
        ));
        drop(pressure);
        let owned = app.selected_text().unwrap().unwrap();
        drop(app);
        assert!(memory.used() > 0);
        assert!(owned.contains("second para"));
        drop(owned);
        assert_eq!(memory.used(), td_alloc::Budget::metadata_bytes());
    }

    #[test]
    fn cut_keeps_the_entry_when_allocation_or_the_clipboard_refuses() {
        let memory = crate::memory::root().unwrap();
        let mut entry = EntryModel::new(512).unwrap();
        let text = "é hello".repeat(30);
        let mut notice = None;
        entry.set_text(&text).unwrap();
        entry.act(Action::SelectAll).unwrap();
        let mut board = Board::default();
        td_alloc::with_reservation_limit(0, || {
            App::edit(&mut entry, "C-x", &mut board, &memory, &mut notice)
        });
        assert_eq!(entry.text(), text);
        assert_eq!(notice.as_deref(), Some("not copied: memory limit"));
        App::edit(
            &mut entry,
            "C-x",
            &mut td_ui::window::NoClipboard,
            &memory,
            &mut notice,
        );
        assert_eq!(entry.text(), text);
        App::edit(&mut entry, "C-x", &mut board, &memory, &mut notice);
        assert_eq!(entry.text(), "");
        assert_eq!(board.copies, [text]);
    }

    /// Same fixture on the merged base and branch; select with TD_COPY_BENCH.
    #[test]
    #[ignore]
    fn copy_benchmark() {
        use std::hint::black_box;
        #[derive(Default)]
        struct Capture {
            legacy: Option<Arc<str>>,
            owned: Option<td_ui::clipboard::OwnedText>,
        }
        impl Clipboard for Capture {
            fn available(&self) -> bool {
                true
            }
            fn has_text(&self) -> bool {
                false
            }
            fn pasting(&self) -> bool {
                false
            }
            fn paste(&mut self) -> Result<(), td_ui::window::Refusal> {
                Ok(())
            }
            fn copy(&mut self, text: Arc<str>) -> Result<(), td_ui::window::Refusal> {
                self.legacy = Some(text);
                Ok(())
            }
            fn copy_owned(
                &mut self,
                text: td_ui::clipboard::OwnedText,
            ) -> Result<(), td_ui::window::Refusal> {
                self.owned = Some(text);
                Ok(())
            }
        }
        impl Capture {
            fn text(&self) -> &str {
                self.owned
                    .as_ref()
                    .map(|text| text.as_str())
                    .or(self.legacy.as_deref())
                    .unwrap()
            }
        }

        let case = std::env::var("TD_COPY_BENCH").unwrap();
        let (kind, body, rounds) = match case.as_str() {
            "short" => ("text/plain", "alpha beta gamma\n".repeat(4), 200_000),
            "large" => ("text/plain", "abcd efgh ijkl mnop\n".repeat(45_000), 300),
            "rich" => (
                "text/html",
                "<p><b>alpha</b> <i>beta</i> <a href='/x'>gamma</a> delta</p>".repeat(8_000),
                600,
            ),
            "unicode" => ("text/plain", "é界 Αβγ café\n".repeat(35_000), 400),
            "entry" => ("text/plain", "hello".into(), 100_000),
            _ => panic!("unknown copy scenario"),
        };
        let mut app = app(site().page("https://example.org/copy", kind, &body));
        typed(&mut app, "example.org/copy");
        if case == "entry" {
            app.focus = Focus::Address;
            app.address.set_text(&"aé".repeat(1200)).unwrap();
            app.address.act(Action::SelectAll).unwrap();
        } else {
            key(&mut app, "C-a");
        }
        let mut board = Capture::default();
        app.copy(&mut board);
        let expected = board.text().to_string();
        let cpu_ns = || -> u64 {
            std::fs::read_to_string("/proc/thread-self/schedstat")
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap()
        };
        let cpu_started = cpu_ns();
        let started = Instant::now();
        let mut count = 0;
        for _ in 0..rounds {
            app.copy(black_box(&mut board));
            let text = black_box(board.text());
            count += text.len();
            assert_eq!(text, expected);
        }
        let cpu_ms = (cpu_ns() - cpu_started) as f64 / 1_000_000.0;
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        let rss = std::fs::read_to_string("/proc/self/status").unwrap();
        let peak = rss.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
        println!(
            "copy-bench {case} ms={elapsed} cpu_ms={cpu_ms} bytes={count} root={} {peak}",
            app.renderer.budget().used()
        );
    }

    #[test]
    fn a_drag_selects_across_wrapped_and_logical_lines_and_copies_them() {
        let mut app = two_paragraphs();
        app.drag_from(point(&app, 0, 6), point(&app, 2, 7));
        // The space the wrap dropped is the page's, and a paragraph ends
        // in one newline.
        assert_eq!(copied(&mut app), ["beta gamma delta epsilon zeta\nsecond "]);
        assert_eq!(app.status_text(), "copied");
        // Dragged up past where it began, it selects before it.
        app.drag_from(point(&app, 1, 8), point(&app, 0, 11));
        assert_eq!(copied(&mut app), ["gamma delta epsilon "]);
        // A plain click clears it.
        app.click_at(point(&app, 2, 1), 0);
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
        // On a continuation, the word and its paragraph; a drag after a
        // double click does not move what it selected.
        let epsilon = point(&app, 1, 2);
        app.click_at(epsilon, 5000);
        app.clock.fixed = Some(5100);
        app.mouse(PointerPhase::Press, epsilon, false);
        app.mouse(PointerPhase::Move, point(&app, 2, 3), false);
        app.mouse(PointerPhase::Release, point(&app, 2, 3), false);
        assert_eq!(copied(&mut app), ["epsilon"]);
        // Off a word, a double click selects nothing.
        let space = point(&app, 0, 5);
        app.click_at(space, 9000);
        app.click_at(space, 9100);
        assert_eq!(app.current.view.selection, None);
        // The word under the pointer, not the caret's: on the right half
        // of a word's last letter, that word; on either half of the space
        // before a word, nothing.
        let half = CELL_WIDTH as i64 - 2;
        let (x, y) = point(&app, 0, 9);
        app.click_at((x + half, y), 12_000);
        app.click_at((x + half, y), 12_100);
        assert_eq!(copied(&mut app), ["beta"]);
        let (x, y) = point(&app, 0, 10);
        app.click_at((x + half, y), 15_000);
        app.click_at((x + half, y), 15_100);
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
        app.mouse(PointerPhase::Press, point(&app, 2, 0), true);
        app.mouse(PointerPhase::Move, point(&app, 2, 1), false);
        app.mouse(PointerPhase::Release, point(&app, 2, 1), false);
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
        let mut app = app(site().page("https://example.org/long", "text/html", &numbered(200)));
        typed(&mut app, "example.org/long");
        let start = point(&app, 0, 0);
        let pane = app.pane();
        let line = pane.line;
        let bottom = pane.rect.y + pane.height();
        assert_eq!(app.wait_ms(0), IDLE_WAIT_MS);
        app.mouse(PointerPhase::Press, start, false);
        app.mouse(PointerPhase::Move, (start.0, bottom), false);
        assert_eq!(app.wait_ms(0), DRAG_SCROLL_MS);
        let y = |app: &App| app.current.view.y;
        let focus = |app: &App| app.current.view.selection.unwrap().focus;
        assert_eq!(y(&app), 0);
        let first = focus(&app);
        app.poll(1000);
        assert_eq!(y(&app), line);
        app.poll(1000 + DRAG_SCROLL_MS - 1);
        assert_eq!(y(&app), line, "not sooner");
        app.poll(1000 + DRAG_SCROLL_MS);
        assert_eq!(y(&app), 2 * line);
        assert!(focus(&app) > first, "the selection follows");
        // Further past the edge, faster, but at most DRAG_SCROLL_LINES.
        app.mouse(PointerPhase::Move, (start.0, bottom + 2 * line), false);
        app.poll(2000);
        assert_eq!(y(&app), 5 * line);
        app.mouse(PointerPhase::Move, (start.0, bottom + 50 * line), false);
        app.poll(3000);
        assert_eq!(y(&app), (5 + DRAG_SCROLL_LINES) * line);
        let anchor = app.current.view.selection.unwrap().anchor;
        assert_eq!(anchor, text::page_bounds(laid(&app)).0);
        // Above the pane it scrolls back, the selection's end at the top.
        app.mouse(PointerPhase::Move, (start.0, pane.rect.y - 1), false);
        app.poll(4000);
        assert_eq!(y(&app), (4 + DRAG_SCROLL_LINES) * line);
        // Inside the pane, or released, it stops.
        app.mouse(PointerPhase::Move, (start.0, pane.rect.y + 1), false);
        app.poll(5000);
        let held = y(&app);
        assert_eq!(held, (4 + DRAG_SCROLL_LINES) * line);
        app.mouse(PointerPhase::Move, (start.0, bottom), false);
        app.mouse(PointerPhase::Release, (start.0, bottom), false);
        assert_eq!(app.wait_ms(0), IDLE_WAIT_MS);
        app.poll(6000);
        assert_eq!(y(&app), held);
        let copy = copied(&mut app);
        assert!(copy[0].starts_with("line 0\nline 1\n"), "{copy:?}");
    }

    /// A Shift press inside the pane's bottom extends the selection there
    /// and scrolls nothing until the pointer moves; a drag held past the
    /// page's end redraws only when it moves something; and the wheel
    /// turned during a drag moves the selection's end to where the
    /// pointer now falls.
    #[test]
    fn only_a_drag_scrolls_and_only_a_change_redraws() {
        let mut app = app(site().page("https://example.org/long", "text/html", &numbered(200)));
        typed(&mut app, "example.org/long");
        let pane = app.pane();
        let bottom = pane.rect.y + pane.height();
        app.mouse(PointerPhase::Press, (40, bottom - 1), true);
        app.poll(1000);
        app.mouse(PointerPhase::Move, (40, bottom - 1), false);
        app.poll(2000);
        app.mouse(PointerPhase::Release, (40, bottom - 1), false);
        assert_eq!(app.current.view.y, 0, "not scrolled");
        assert!(app.current.view.selection.is_some());
        // At the page's end a drag below it scrolls nothing and asks for
        // no paint.
        key(&mut app, "End");
        let y = app.current.view.y;
        app.mouse(PointerPhase::Press, (40, pane.rect.y + 1), false);
        app.mouse(PointerPhase::Move, (40, bottom), false);
        app.poll(3000);
        app.redraw = false;
        app.poll(3100);
        assert!(!app.redraw);
        assert_eq!(app.current.view.y, y);
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
        assert_eq!(app.current.view.y, y - 3 * pane.line);
        let now = app.current.view.selection.unwrap().focus;
        assert!(now < first, "{now:?} {first:?}");
        app.mouse(PointerPhase::Release, (40, pane.rect.y + 1), false);
    }

    /// A pane shorter than a line does not scroll by a drag.
    #[test]
    fn a_pane_shorter_than_a_line_does_not_drag_scroll() {
        let mut app = app(site().page("https://example.org/long", "text/html", &numbered(50)));
        typed(&mut app, "example.org/long");
        key(&mut app, "PageDown");
        let height = 2 * td_ui::chrome::ROW + 8;
        let mut board = Board::default();
        app.input(
            Input::Resize(Surface::new(800, height, Scale::new(1).unwrap()).unwrap()),
            &mut board,
        );
        relaid(&mut app);
        let pane = app.pane();
        assert!(pane.height() < pane.line);
        let y = app.current.view.y;
        assert!(y > 0);
        app.mouse(PointerPhase::Press, (40, pane.rect.y + 1), true);
        app.mouse(PointerPhase::Move, (40, pane.rect.y + 40), false);
        assert_eq!(app.wait_ms(0), IDLE_WAIT_MS);
        app.poll(1000);
        assert_eq!(app.current.view.y, y);
        app.mouse(PointerPhase::Release, (40, pane.rect.y + 40), false);
        assert!(app.current.view.selection.is_some());
    }

    #[test]
    fn a_selection_survives_scrolling_a_resize_and_f8() {
        // One paragraph of some 400 lines, read 30 lines in.
        let words: Vec<String> = (0..10_000).map(|n| format!("w{n}")).collect();
        let page = format!("<div>{}</div>", words.join(" "));
        let mut app = app(site().page("https://example.org/p", "text/html", &page));
        typed(&mut app, "example.org/p");
        for _ in 0..30 {
            key(&mut app, "Down");
        }
        let line = top_line(&app) + 1;
        app.drag_from(point(&app, line, 0), point(&app, line + 2, 3));
        let selection = app.current.view.selection;
        let text = copied(&mut app);
        assert!(text[0].len() > 100, "{text:?}");
        let words: String = text[0].split(' ').collect();
        assert_eq!(painted_selection(&mut app), words);
        key(&mut app, "End");
        assert_eq!(painted_selection(&mut app), "");
        for change in [Some(300), None, Some(800), None] {
            match change {
                Some(width) => resize(&mut app, width),
                None => {
                    key(&mut app, "F8");
                    relaid(&mut app);
                }
            }
            assert_eq!(app.current.view.selection, selection);
            assert_eq!(copied(&mut app), text);
        }
        key(&mut app, "Home");
        for _ in 0..30 {
            key(&mut app, "Down");
        }
        assert_eq!(painted_selection(&mut app), words);
    }

    type SearchState = (
        i64,
        Option<usize>,
        usize,
        Option<Selection>,
        Vec<crate::search::Hit>,
    );

    fn search_state(app: &App) -> SearchState {
        let view = &app.current.view;
        (
            view.y,
            view.focus,
            view.at,
            view.selection,
            view.hits().to_vec(),
        )
    }

    #[test]
    fn a_refused_query_keeps_old_results_and_enter_retries_the_new_query() {
        let mut app = app(site().page(
            "https://example.org/search",
            "text/html",
            "<p>alpha beta beta a-long-target</p>",
        ));
        typed(&mut app, "example.org/search");
        app.focus = Focus::Find;
        app.query.set_text("alpha").unwrap();
        app.refind();
        let before = search_state(&app);
        app.query.set_text("beta").unwrap();
        td_alloc::with_reservation_limit(0, || app.refind());
        assert!(app.current.find_refused);
        assert_eq!(search_state(&app), before);
        // Unrelated navigation cannot dismiss the failed-query indication.
        app.scroll(0);
        assert!(app.current.find_refused);
        key(&mut app, "Return");
        assert!(!app.current.find_refused);
        assert_eq!(app.current.view.hits().len(), 2);
        assert_eq!(
            app.current.view.at, 0,
            "retry does not skip the first match"
        );
        assert_eq!(app.selected_text().unwrap().unwrap(), "beta");
        app.query.set_text("a-long-target").unwrap();
        td_alloc::with_reservation_limit(0, || app.refind());
        assert!(app.current.find_refused);
        key(&mut app, "Escape");
        key(&mut app, "n");
        assert!(!app.current.find_refused);
        assert_eq!(app.current.view.hits().len(), 1);
        key(&mut app, "Escape");
        assert!(app.current.view.hits.is_none(), "clear releases storage");
    }

    #[test]
    fn reflow_keeps_a_refused_query_pending_until_find_and_reveal_succeed() {
        for change in ["resize", "font", "refused-resize"] {
            for retry in ["Return", "n"] {
                let mut app = app(site().page(
                    "https://example.org/search",
                    "text/html",
                    "<p>alpha beta beta</p>",
                ));
                typed(&mut app, "example.org/search");
                app.focus = Focus::Find;
                app.query.set_text("alpha").unwrap();
                app.refind();
                let old = app.current.view.hits().to_vec();
                let selected = app.current.view.selection;
                app.query.set_text("beta").unwrap();
                td_alloc::with_reservation_limit(0, || app.refind());
                assert!(app.current.find_refused);
                match change {
                    "font" => key(&mut app, "F8"),
                    "refused-resize" => td_alloc::with_reservation_limit(0, || {
                        app.input(Input::Resize(surface(300)), &mut Board::default());
                    }),
                    _ => {
                        app.input(Input::Resize(surface(300)), &mut Board::default());
                    }
                }
                relaid(&mut app);
                assert_eq!(laid(&app).want, app.want(), "{change}");
                assert!(
                    app.current.find_refused,
                    "{change}: pending query was dismissed"
                );
                assert_eq!(app.current.view.hits(), old);
                assert_eq!(app.current.view.selection, selected);
                if retry == "n" {
                    key(&mut app, "Escape");
                }
                key(&mut app, retry);
                assert!(!app.current.find_refused, "{change}/{retry}");
                assert_eq!(
                    app.current.view.at, 0,
                    "{change}/{retry}: first result was skipped"
                );
                assert_eq!(app.current.view.hits().len(), 2);
                assert_eq!(app.selected_text().unwrap().unwrap(), "beta");
            }
        }
    }

    /// However few reservations a resize is allowed, the page is laid for
    /// the new width once the window polls again, find's matches, its
    /// current one and the selection as an unrefused resize leaves them.
    #[test]
    fn every_refused_resize_preserves_search_results_until_reflow_commits() {
        let text = format!("<pre>needle {} needle</pre>", "x".repeat(50)).repeat(10);
        let prepared = || {
            let mut app = app(site().page("https://example.org/search", "text/html", &text));
            typed(&mut app, "example.org/search");
            app.focus = Focus::Find;
            app.query.set_text("needle").unwrap();
            app.refind();
            assert_eq!(app.current.view.hits().len(), 20);
            key(&mut app, "S-Return");
            app
        };
        let mut expected = prepared();
        resize(&mut expected, 300);
        assert_eq!(expected.current.view.at, 19);
        let wanted = search_state(&expected);
        for n in 0..32 {
            let mut app = prepared();
            let before = search_state(&app);
            td_alloc::with_reservation_limit(n, || {
                app.input(Input::Resize(surface(300)), &mut Board::default());
            });
            assert_eq!(search_state(&app), before, "allowance {n}");
            relaid(&mut app);
            assert_eq!(laid(&app).want, app.want(), "allowance {n}");
            assert_eq!(search_state(&app), wanted, "allowance {n}");
        }
    }

    /// Extending the selection by a key takes no reservation: with none
    /// allowed it moves as it does with any, from the top's place or from
    /// the selection, showing where its end went.
    #[test]
    fn keyboard_extension_takes_no_reservation() {
        let text = "alpha beta gamma delta ".repeat(2000);
        let mut app = app(site().page("https://example.org/p", "text/plain", &text));
        typed(&mut app, "example.org/p");
        let (start, end) = text::page_bounds(laid(&app));
        td_alloc::with_reservation_limit(0, || key(&mut app, "S-Right"));
        let first = Place {
            offset: start.offset + 1,
            ..start
        };
        assert_eq!(
            app.current.view.selection,
            Some(Selection {
                anchor: start,
                focus: first
            })
        );
        td_alloc::with_reservation_limit(0, || key(&mut app, "S-Down"));
        let selection = app.current.view.selection.unwrap();
        assert_eq!((selection.anchor, selection.focus), (start, end));
        assert!(app.current.view.y > 0, "its end shown");
    }

    /// A resize refused keeps the page, the view and what a copy takes,
    /// and is asked again until it is laid.
    #[test]
    fn refused_resize_keeps_the_page_view_and_copy_then_retries() {
        let text = "alpha beta gamma delta ".repeat(2000);
        let mut app = app(site().page("https://example.org/p", "text/plain", &text));
        typed(&mut app, "example.org/p");
        for _ in 0..20 {
            key(&mut app, "Down");
        }
        let line = top_line(&app) + 1;
        app.drag_from(point(&app, line, 0), point(&app, line + 1, 3));
        let before = (app.current.view.y, app.current.view.selection);
        let text = copied(&mut app);
        assert!(!text.is_empty());
        for allowance in [0, 1, 3, 8] {
            td_alloc::with_reservation_limit(allowance, || {
                app.input(Input::Resize(surface(300)), &mut Board::default());
            });
            assert_eq!((app.current.view.y, app.current.view.selection), before);
            assert_eq!(copied(&mut app), text);
            relaid(&mut app);
            assert_eq!(laid(&app).want, app.want(), "allowance {allowance}");
            assert_eq!(app.current.view.selection, before.1);
            assert_eq!(copied(&mut app), text);
            resize(&mut app, 800);
            assert_eq!((app.current.view.y, app.current.view.selection), before);
        }
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
        app.mouse(PointerPhase::Move, point(&app, 2, 3), false);
        app.mouse(PointerPhase::Release, point(&app, 2, 3), false);
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
        // Up to the last line's start, it is within the bound. (The newline
        // before that start is not copied: see
        // `shift_keys_step_by_scalar_word_and_paragraph`.)
        key(&mut app, "S-Up");
        let copy = copied(&mut app);
        let within = (lines - 1) * 100;
        assert!(copy[0].len() <= MAX_COPY);
        assert_eq!(copy[0].trim_end_matches('\n'), &body[..within - 1]);
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
            ("S-Left", "beta"),
            ("S-Up", "alpha "),
            ("S-Up", "alpha "),
            ("S-Right", "lpha "),
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

    /// The keys move the selection's end by DESIGN.md §8: a scalar, from
    /// a paragraph's end to the next one's start a step; to the end of
    /// the word after or the start of the one before, within the
    /// paragraph; to the paragraph's end, then the next one's; and a
    /// selection to a paragraph's start copies the newline before it.
    #[test]
    fn shift_keys_step_by_scalar_word_and_paragraph() {
        let page = "<p>alpha beta gamma</p><p>delta</p>";
        let mut app = app(site().page("https://example.org/f", "text/html", page));
        typed(&mut app, "example.org/f");
        app.drag_from(point(&app, 0, 6), point(&app, 0, 10));
        assert_eq!(copied(&mut app), ["beta"]);
        for (chord, copy) in [
            ("S-Right", "beta "),
            ("C-S-Right", "beta gamma"),
            ("S-Right", "beta gamma\n"),
            ("S-Left", "beta gamma"),
            ("S-Down", "beta gamma\ndelta"),
            ("S-Up", "beta gamma\n"),
            ("S-Left", "beta gamma"),
            ("C-S-Left", "beta "),
        ] {
            key(&mut app, chord);
            assert_eq!(copied(&mut app), [copy], "{chord}");
        }
    }

    /// A line that is not wrapped is laid whole, past the pane's edge, and
    /// a triple click copies it whole.
    #[test]
    fn a_line_not_wrapped_copies_its_kept_text_past_the_panes_edge() {
        let page = format!("<pre>{}END</pre><p>after</p>", "x".repeat(300));
        let mut app = app(site().page("https://example.org/pre", "text/html", &page));
        typed(&mut app, "example.org/pre");
        let (left, right) = text::horizontal(&laid(&app).lines()[0]);
        assert_eq!(right - left, (303 * CELL_WIDTH) as f64);
        let at = point(&app, 0, 5);
        for ms in [1000, 1100, 1200] {
            app.click_at(at, ms);
        }
        let whole = format!("{}END", "x".repeat(300));
        assert_eq!(copied(&mut app), std::slice::from_ref(&whole));
        // A drag to the pane's right edge selects to the scalar there;
        // scrolled across, the rest.
        let pane = app.pane();
        let right = pane.rect.x + i64::from(pane.rect.width) - 1;
        app.drag_from(point(&app, 0, 0), (right, at.1));
        let partly = copied(&mut app);
        assert!(partly[0].len() < 300 && partly[0].len() > 50, "{partly:?}");
        key(&mut app, "End");
        for _ in 0..100 {
            key(&mut app, "Right");
        }
        app.shift_click(point(&app, 0, 303));
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

    /// The shapes `measure_window` lays beside the corpus: td-layout's
    /// 2 MiB shapes, td-html's hostile ones, hostile sheets and many faces
    /// and sizes on one page.
    fn shapes() -> Vec<(&'static str, String)> {
        const MOST: usize = 2 << 20;
        let filled = |head: &str, unit: &str| {
            let mut out = String::from(head);
            while out.len() + unit.len() <= MOST {
                out.push_str(unit);
            }
            while out.len() < MOST {
                out.push('x');
            }
            out
        };
        let paragraphs = "<p>text".repeat(5000);
        let mut not = String::from("<style>");
        for n in 0..5000 {
            not.push_str(&format!(":not(.c{n}) {{ margin: 0 }}"));
        }
        not.push_str("</style>");
        not.push_str(&paragraphs);
        let var = format!(
            "<style>:root {{ --v: {} }} * {{ margin-left: var(--v) }}</style>{paragraphs}",
            "a ".repeat(30_000)
        );
        let custom = |bytes: usize| {
            let mut custom = String::from("<style>:root {");
            let mut n = 0;
            while custom.len() < bytes {
                custom.push_str(&format!("--p{n}: {};", "y".repeat(200)));
                n += 1;
            }
            custom.push_str("}</style>");
            custom.push_str(&paragraphs);
            custom
        };
        let mut faces = String::new();
        for family in ["serif", "sans-serif", "monospace"] {
            for style in ["normal", "italic"] {
                for weight in ["normal", "bold"] {
                    for size in (6..=200).step_by(2) {
                        faces.push_str(&format!(
                            "<span style=\"font: {style} {weight} {size}px {family}\">\
                             Quick brown fox {size} </span>"
                        ));
                    }
                }
            }
        }
        vec![
            ("2 MiB prose", filled("<p>", "words here ")),
            ("2 MiB word", filled("<p>", "x")),
            ("2 MiB blocks", filled("<body>", "<p>abcdefghijklmnop</p>")),
            (
                "one-letter paragraphs in 62 quotes",
                filled(&"<blockquote>".repeat(62), "<p>a"),
            ),
            ("list items in 62", filled(&"<ul><li>".repeat(62), "<li>a")),
            ("5,000 universal :not() rules", not),
            ("a var() of 30,000 tokens on every element", var),
            ("1.5 MiB of custom properties", custom(3 << 19)),
            ("0.9 MiB of custom properties", custom(900 << 10)),
            ("many families and sizes", faces),
            (
                "30 lines of 60,000 scalars at 6px",
                format!(
                    "<pre style=\"font-size:6px\">{}</pre>",
                    format!("{}\n", "a".repeat(60_000)).repeat(30)
                ),
            ),
            (
                "500,000 lines of no height",
                format!("<pre style=line-height:0>{}</pre>", "a\n".repeat(500_000)),
            ),
            (
                "2,000 stacked boxes",
                format!(
                    "<style>div{{height:3000px;margin-bottom:-3000px;background:#123456}}</style>{}",
                    "<div></div>".repeat(2000)
                ),
            ),
        ]
    }

    /// The process's resident peak, VmHWM, in MiB.
    fn resident_peak() -> f64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let line = status.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
        let kib: f64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
        kib / 1024.0
    }

    /// Waits as long as `done` takes, polling.
    fn wait(app: &mut App, mut done: impl FnMut(&App) -> bool) {
        for _ in 0..600_000 {
            app.poll(0);
            if done(app) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
        panic!("waited a minute");
    }

    /// The window over the pinned faces at 1920x1200 at scale one and
    /// 3840x2160 at two: each page of `TD_WEB_CORPUS` (captured to
    /// scratch, never committed) and each of `shapes` made, painted whole
    /// twice, laid out again two thirds as wide and painted; then, with
    /// the last page shown, the largest made beside it. `TD_WEB_ONLY`
    /// names one page or shape and `TD_WEB_SCALE` one scale, so a process
    /// of its own reports its resident peak. A corpus with a `pages.tsv`
    /// (`fetch::tests::Corpus`) is asked for at its pages' own URLs, its
    /// sheets with them, each answer taking its captured time under
    /// `TD_WEB_REPLAY`, and `TD_WEB_STYLE_WAIT` milliseconds in place of
    /// `STYLE_WAIT`; the URLs asked for and not captured are written to
    /// its `missing.txt`. Release build: `XDG_DATA_HOME=<install> cargo
    /// test --release measure_window -- --ignored --nocapture`.
    #[test]
    #[ignore = "host measurement: the pinned faces, TD_WEB_CORPUS, release build"]
    fn measure_window() {
        use std::time::Instant;
        use td_ui::face_file::Family;
        let mono = td_ui::pinned_face::load_in(&pinned(Family::MONO)).unwrap();
        let faces = td_ui::pinned_face::prose_in(&pinned(Family::SANS))
            .unwrap()
            .with_mono(&mono)
            .unwrap();
        let font = td_ui::font::pinned().unwrap();
        let mut pages: Vec<(String, String)> = Vec::new();
        let dir = std::env::var_os("TD_WEB_CORPUS").map(PathBuf::from);
        let corpus = dir
            .as_deref()
            .and_then(crate::fetch::tests::Corpus::read)
            .map(Arc::new);
        if let Some(corpus) = &corpus {
            pages.extend(corpus.pages.iter().cloned());
        } else if let Some(dir) = &dir {
            let mut names: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|e| e == "html"))
                .collect();
            names.sort();
            for path in names {
                let bytes = std::fs::read(&path).unwrap();
                let name = path.file_stem().unwrap().to_string_lossy().into_owned();
                pages.push((name, String::from_utf8_lossy(&bytes).into_owned()));
            }
        }
        if corpus.is_none() {
            pages.extend(shapes().into_iter().map(|(n, t)| (n.to_string(), t)));
        }
        let only = std::env::var("TD_WEB_ONLY").ok();
        pages.retain(|(name, _)| only.as_ref().is_none_or(|only| name == only));
        let scales: Vec<(usize, usize, u8)> = match std::env::var("TD_WEB_SCALE").as_deref() {
            Ok("1") => vec![(1920, 1200, 1)],
            Ok("2") => vec![(3840, 2160, 2)],
            _ => vec![(1920, 1200, 1), (3840, 2160, 2)],
        };
        // A shape's address is its index; a corpus page's its URL.
        let mut table = Table::default();
        let mut addresses = Vec::new();
        for (n, (_, text)) in pages.iter().enumerate() {
            let url = corpus
                .as_ref()
                .filter(|c| c.pages.iter().any(|(_, url)| url == text));
            if url.is_some() {
                addresses.push(text.clone());
                continue;
            }
            addresses.push(format!("example.org/{n}"));
            table = table.page(&format!("https://example.org/{n}"), "text/html", text);
        }
        let table: crate::fetch::Shared = match &corpus {
            Some(corpus) => corpus.clone(),
            None => Arc::new(table),
        };
        let style_wait = std::env::var("TD_WEB_STYLE_WAIT")
            .ok()
            .map(|ms| std::time::Duration::from_millis(ms.parse().unwrap()));
        for (width, height, scale) in scales {
            let surface = Surface::new(width, height, Scale::new(scale).unwrap()).unwrap();
            let mut pixels = vec![0u8; width * height * 4];
            for (n, (name, _)) in pages.iter().enumerate() {
                let mut app = App::new(table.clone(), surface, Some(faces.clone())).unwrap();
                if let Some(style_wait) = style_wait {
                    app.style_wait = style_wait;
                }
                if let Ok(slots) = std::env::var("TD_WEB_PICTURE_SLOTS") {
                    app.picture_slots = slots.parse().unwrap();
                }
                if let Ok(term) = std::env::var("TD_WEB_PICTURES_TERM") {
                    app.pictures_term = term.parse::<usize>().unwrap() << 20;
                }
                let alone = app.renderer.budget().used();
                type_address(&mut app, &addresses[n]);
                let started = Instant::now();
                let mut styles = None;
                wait(&mut app, |app| {
                    if styles.is_none() && app.status_text().starts_with("loading styles") {
                        styles = Some(Instant::now());
                    }
                    app.loading.is_none()
                });
                let made = started.elapsed();
                let waited = styles.map(|styles| styles.elapsed());
                // Its pictures, as they come: how many settles laid them
                // and when the last was laid.
                let shown = Instant::now();
                let (mut settles, mut settled) = (0, 0);
                let (mut relaying, mut relaid) = (None, std::time::Duration::ZERO);
                wait(&mut app, |app| {
                    let pictures = app.current.pictures.as_ref();
                    let now = pictures.map_or(0, Pictures::settled);
                    if now != settled {
                        settled = now;
                        settles += 1;
                    }
                    match (relaying, app.current.relayout.is_some()) {
                        (None, true) => relaying = Some(Instant::now()),
                        (Some(since), false) => {
                            relaid += since.elapsed();
                            relaying = None;
                        }
                        _ => {}
                    }
                    !pictures.is_some_and(Pictures::busy)
                        && app.current.relayout.is_none()
                        && !app.current.again
                });
                if let Some(pictures) = app.current.pictures.as_ref() {
                    let why: Vec<_> = (0..pictures::MAX_PICTURES)
                        .filter_map(|at| pictures.refused(at))
                        .collect();
                    println!(
                        "{name} at {scale}: pictures {:?}, refused {why:?}, laid by {settles} \
                         settles taking {relaid:.1?}, the last {:.1?} after the page, the term \
                         holding {:.2} MiB",
                        pictures.count(|node| !app
                            .current
                            .laid()
                            .is_some_and(|laid| laid.refused(node))),
                        shown.elapsed(),
                        pictures.term().used() as f64 / 1048576.0,
                    );
                }
                let held = app.renderer.budget().used() - alone;
                if let Some(sheets) = app.current.doc.page().and_then(|p| p.sheets.as_ref()) {
                    println!(
                        "{name}: {} sheets linked and imported, {:?}, {:.1} KiB held, \
                         waited for them {waited:.1?}",
                        sheets.taken(),
                        sheets.tally(),
                        sheets.bytes() as f64 / 1024.0,
                    );
                }
                let Some(laid) = app.current.laid().and_then(|l| l.try_clone().ok()) else {
                    println!("{name} at {scale}: no page ({})", app.status_text());
                    continue;
                };
                let rung = app.current.doc.page().map(|page| page.rung);
                let mut paint = |app: &mut App, faces: &mut Faces| {
                    let started = Instant::now();
                    let mut raster =
                        Raster::new(&mut pixels, &font, app.surface, app.surface.width * 4)
                            .unwrap()
                            .with_prose(Some(faces));
                    app.paint(&mut raster, app.surface).unwrap();
                    started.elapsed()
                };
                let mut faces = faces.clone();
                let cold = paint(&mut app, &mut faces);
                let warm = paint(&mut app, &mut faces);
                let narrower =
                    Surface::new(width * 2 / 3, height, Scale::new(scale).unwrap()).unwrap();
                let started = Instant::now();
                app.input(Input::Resize(narrower), &mut Board::default());
                wait(&mut app, |app| {
                    app.current.relayout.is_none() && !app.current.again
                });
                let relaid = started.elapsed();
                let narrow = paint(&mut app, &mut faces);
                println!(
                    "{name} at {width}x{height}: {rung:?}, {} lines, made {made:.1?}, \
                     painted {cold:.1?} cold / {warm:.1?} warm, relaid {relaid:.1?} \
                     ({}), painted {narrow:.1?}, held {:.1} MiB, page_bytes {:.1} MiB, \
                     status \"{}\", resident peak {:.1} MiB",
                    laid.layout.line_count,
                    if app.current.kept { "old kept" } else { "ok" },
                    held as f64 / 1048576.0,
                    laid.layout.page_bytes as f64 / 1048576.0,
                    app.status_text(),
                    resident_peak(),
                );
                drop(laid);
                // The largest made beside it, when asked for.
                if let Ok(beside) = std::env::var("TD_WEB_BESIDE") {
                    let other = pages.iter().position(|(name, _)| *name == beside);
                    if let Some(other) = other {
                        type_address(&mut app, &addresses[other]);
                        wait(&mut app, |app| app.loading.is_none());
                        let _ = paint(&mut app, &mut faces);
                        println!(
                            "  then {beside} made beside it: status \"{}\", resident peak {:.1} MiB",
                            app.status_text(),
                            resident_peak()
                        );
                    }
                }
            }
        }
        if let (Some(corpus), Some(dir)) = (&corpus, &dir) {
            println!("{} URLs asked for and not captured", corpus.missing(dir));
        }
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
        assert_eq!(line_text(&app, 0), "iiii mmmm a link after");
        // Each scalar's extent on a line, replayed through the layout's
        // metrics, in the window's pixels.
        let extents = |app: &mut App, at: usize| {
            let pane = app.pane();
            let frame = app.current.view.frame(&pane);
            let (laid, measure) =
                measure_of(&app.current.doc, &mut app.current.measure, &app.renderer).unwrap();
            let line = &laid.lines()[at];
            let mut out = Vec::new();
            for f in line.fragments.iter() {
                text::extents(&laid, measure, line, f, |e| {
                    let x = frame.x + (e.x * pane.scale as f64).round() as i64;
                    let end = frame.x + ((e.x + e.width) * pane.scale as f64).round() as i64;
                    out.push((x, end, e.shown));
                });
            }
            out
        };
        let first = extents(&mut app, 0);
        let shown: String = first.iter().map(|&(_, _, c)| c).collect();
        assert_eq!(shown, "iiii mmmm a link after");
        let advance = |at: usize| first[at].1 - first[at].0;
        assert!(
            advance(0) * 2 < advance(5),
            "i {} m {}",
            advance(0),
            advance(5)
        );
        // The pointer over the link names it; past it, nothing.
        let y = point(&app, 0, 0).1;
        let (start, end) = (first[10].0, first[15].1);
        app.motion(Some(((start + end) / 2, y)));
        assert_eq!(app.status_text(), "https://example.org/a");
        app.motion(Some((end + 2, y)));
        assert_eq!(app.hover, None);
        // A drag selects by the glyphs' pixels: from the left half of the
        // first 'm' to the left half of the space after the last; and a
        // double click on an 'i' selects its word.
        let pen = |at: usize| first[at].0 + 1;
        app.drag_from((pen(5), y), (pen(9), y));
        assert_eq!(copied(&mut app), ["mmmm"]);
        app.click_at((pen(2), y), 1000);
        app.click_at((pen(2), y), 1100);
        assert_eq!(copied(&mut app), ["iiii"]);
        // A match is painted over its glyphs' pixels.
        key(&mut app, "/");
        for c in "after".chars() {
            key(&mut app, &c.to_string());
        }
        assert_eq!(app.current.view.hits().len(), 1);
        let marked: Vec<(i64, char)> = painted(&mut app)
            .into_iter()
            .filter(|&(_, _, selected)| selected)
            .map(|(x, c, _)| (x, c))
            .collect();
        let after: String = marked.iter().map(|&(_, scalar)| scalar).collect();
        assert_eq!(after, "after");
        assert_eq!(marked[0].0, first[17].0);
        key(&mut app, "Escape");
        // A preformatted line is in the monospace face, one advance a
        // glyph.
        let pre = extents(&mut app, 1);
        let steps: Vec<i64> = pre.iter().map(|&(x, end, _)| end - x).collect();
        let (low, high) = (steps.iter().min().unwrap(), steps.iter().max().unwrap());
        assert!(high - low <= 1, "{steps:?}");
        // Serif measures otherwise than Sans.
        let sans = text::horizontal(&laid(&app).lines()[0]);
        key(&mut app, "F8");
        relaid(&mut app);
        assert_eq!(laid(&app).want.body, TextFamily::Serif);
        assert_ne!(text::horizontal(&laid(&app).lines()[0]), sans);
        // And the window's raster, lent the same faces, draws it.
        let mut faces = faces;
        let font = td_ui::font::pinned().unwrap();
        let surface = app.surface;
        let page = app.page_rect();
        let left = page.x + app.inset();
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
        let top = point(&app, 0, 0).1 - 1;
        let ink = (top..top + 22).any(|y| {
            let row = row(y);
            row.chunks(4).any(|pixel| pixel != &row[..4])
        });
        assert!(ink, "the first line is drawn");
    }

    // Stylesheets (DESIGN.md §10, "Subresources"), through the injected
    // fetcher.

    /// Answers from a table, recording each request as it starts and
    /// holding those to `held` URLs until the test releases one.
    struct Gate {
        table: Table,
        held: Vec<String>,
        started: Mutex<Vec<String>>,
        release: Mutex<mpsc::Receiver<()>>,
        /// URLs held apart, each released by its own sender.
        apart: Vec<(String, Mutex<mpsc::Receiver<()>>)>,
    }

    impl Fetcher for Gate {
        fn get(&self, url: &str, ask: Ask) -> Result<Reply, String> {
            self.started.lock().unwrap().push(url.into());
            if let Some((_, release)) = self.apart.iter().find(|(apart, _)| apart == url) {
                let _ = release.lock().unwrap().recv();
            } else if self.held.iter().any(|held| held == url) {
                let _ = self.release.lock().unwrap().recv();
            }
            self.table.get(url, ask)
        }
    }

    fn gate(table: Table, held: &[&str]) -> (Arc<Gate>, mpsc::Sender<()>) {
        let (gate, release, _) = gate_apart(table, held, &[]);
        (gate, release)
    }

    /// `gate`, with each of `apart` held until its own sender releases
    /// it.
    fn gate_apart(
        table: Table,
        held: &[&str],
        apart: &[&str],
    ) -> (Arc<Gate>, mpsc::Sender<()>, Vec<mpsc::Sender<()>>) {
        let (release, released) = mpsc::channel();
        let mut senders = Vec::new();
        let mut receivers = Vec::new();
        for url in apart {
            let (send, receive) = mpsc::channel();
            senders.push(send);
            receivers.push((url.to_string(), Mutex::new(receive)));
        }
        let gate = Arc::new(Gate {
            table,
            held: held.iter().map(|url| url.to_string()).collect(),
            started: Mutex::new(Vec::new()),
            release: Mutex::new(released),
            apart: receivers,
        });
        (gate, release, senders)
    }

    fn started(gate: &Gate) -> Vec<String> {
        gate.started.lock().unwrap().clone()
    }

    /// The page's status row with no message over it.
    fn notes(app: &mut App) -> String {
        app.message = None;
        app.status_text().into_owned()
    }

    /// Waits for every subresource fetch to end.
    fn slots_free(app: &App) {
        for _ in 0..1000 {
            if app.slots.out() == 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("a subresource fetch did not end");
    }

    #[test]
    fn linked_and_imported_sheets_apply_in_cascade_order() {
        let table = Table::default()
            .page(
                "https://example.org/",
                "text/html",
                "<link rel=stylesheet href=/a.css>\
                 <style>@import 'b.css'; .two{display:inline} .three{display:none}</style>\
                 <link rel=stylesheet href=c.css>\
                 <span class=one>1</span><span class=two>2</span><span class=three>3</span>",
            )
            .sheet(
                "https://example.org/a.css",
                "@import 'i.css'; .one{display:none}",
            )
            .sheet(
                "https://example.org/i.css",
                ".one{display:inline} .two{display:none}",
            )
            .sheet("https://example.org/b.css", ".three{display:inline}")
            .sheet("https://example.org/c.css", ".three{display:inline}");
        let table = Arc::new(table);
        let mut app = app_over(table.clone());
        typed(&mut app, "example.org");
        // i.css comes before a.css, which hides one; the style element
        // shows two after i.css hid it, and c.css shows three again.
        assert_eq!(page_text(&app), "23");
        let asked = table.asked.lock().unwrap().clone();
        let sheets: Vec<_> = asked
            .iter()
            .filter(|(url, _)| url.ends_with(".css"))
            .collect();
        assert_eq!(sheets.len(), 4, "{asked:?}");
        for (_, ask) in sheets {
            assert_eq!(
                *ask,
                Ask {
                    public: true,
                    accept: "text/css,*/*;q=0.1",
                    limit: 1 << 20,
                    deadline: ask.deadline,
                }
            );
            // Within the page's STYLE_WAIT.
            assert!(ask.deadline.is_some());
        }
        assert_eq!(notes(&mut app), "");
        // Kept for a new environment: a resize compiles them again.
        resize(&mut app, 600);
        assert_eq!(page_text(&app), "23");
        // And F9 leaves them out, then takes them again.
        key(&mut app, "F9");
        relaid(&mut app);
        assert_eq!(page_text(&app), "123");
        key(&mut app, "F9");
        relaid(&mut app);
        assert_eq!(page_text(&app), "23");
    }

    #[test]
    fn every_sheet_is_fetched_and_applied_only_where_its_chains_media_match() {
        let table = Table::default()
            .page(
                "https://example.org/",
                "text/html",
                "<link rel=stylesheet href=p.css media=print>\
                 <link rel=stylesheet href=m.css media='screen'>\
                 <span class=x>x</span><span class=y>y</span><span class=z>z</span>",
            )
            .sheet("https://example.org/p.css", ".x{display:none}")
            .sheet(
                "https://example.org/m.css",
                "@import 'n.css' print; @import url(o.css) screen and (min-width: 1px);",
            )
            .sheet("https://example.org/n.css", ".y{display:none}")
            .sheet("https://example.org/o.css", ".z{display:none}");
        let table = Arc::new(table);
        let mut app = app_over(table.clone());
        typed(&mut app, "example.org");
        assert_eq!(page_text(&app), "xy");
        let urls = table.urls();
        for sheet in ["p.css", "m.css", "n.css", "o.css"] {
            assert!(urls.iter().any(|url| url.ends_with(sheet)), "{urls:?}");
        }
    }

    #[test]
    fn imports_nest_four_deep_and_the_count_is_bounded() {
        let mut table = Table::default().page(
            "https://example.org/",
            "text/html",
            "<link rel=stylesheet href=/0.css>\
             <b class=c0>0</b><b class=c1>1</b><b class=c2>2</b><b class=c3>3</b>\
             <b class=c4>4</b><b class=c5>5</b>",
        );
        for n in 0..6 {
            table = table.sheet(
                &format!("https://example.org/{n}.css"),
                &format!("@import '/{}.css'; .c{n}{{display:none}}", n + 1),
            );
        }
        let table = Arc::new(table);
        let mut app = app_over(table.clone());
        typed(&mut app, "example.org");
        // 0.css links, 1.css to 4.css import four deep; 5.css is past.
        assert_eq!(page_text(&app), "5");
        assert!(!table.urls().iter().any(|url| url.ends_with("/5.css")));
        assert_eq!(
            notes(&mut app),
            "1 of the page's style sheets was not applied"
        );
        // MAX_SHEET_FETCHES linked; one more is not fetched.
        let mut markup = String::new();
        let mut table = Table::default();
        for n in 0..=crate::gather::MAX_SHEET_FETCHES {
            markup.push_str(&format!("<link rel=stylesheet href=/s{n}.css>"));
            table = table.sheet(&format!("https://example.org/s{n}.css"), "");
        }
        markup.push_str("<p>shown");
        let table = Arc::new(table.page("https://example.org/", "text/html", &markup));
        let mut app = app_over(table.clone());
        typed(&mut app, "example.org");
        assert_eq!(page_text(&app), "shown");
        let fetched = table.urls().iter().filter(|u| u.ends_with(".css")).count();
        assert_eq!(fetched, crate::gather::MAX_SHEET_FETCHES);
        assert_eq!(
            notes(&mut app),
            "1 of the page's style sheets was not applied"
        );
    }

    #[test]
    fn sheets_past_the_byte_bounds_are_left_out() {
        let big = format!(".a{{display:none}}{}", " ".repeat(1 << 20));
        // A byte of windows-1252 decodes to two of UTF-8 here: within the
        // service's limit, past td-css's once decoded.
        let mut wide = b".b{display:none}".to_vec();
        wide.extend(std::iter::repeat_n(0xe9u8, (1 << 20) - wide.len()));
        let near = format!(".c{{display:none}}/*{}*/", "x".repeat(900 << 10));
        let mut table = Table::default()
            .sheet("https://example.org/big.css", &big)
            .reply(
                "https://example.org/wide.css",
                200,
                &[("content-type", "text/css; charset=windows-1252")],
                &wide,
            );
        let mut markup =
            String::from("<link rel=stylesheet href=big.css><link rel=stylesheet href=wide.css>");
        // Five of 0.9 MiB pass the 4 MiB a page's sheets hold together.
        for n in 0..5 {
            markup.push_str(&format!("<link rel=stylesheet href=n{n}.css>"));
            table = table.sheet(&format!("https://example.org/n{n}.css"), &near);
        }
        markup.push_str("<b class=a>a</b><b class=b>b</b><b class=c>c</b>");
        let table = table.page("https://example.org/", "text/html", &markup);
        let mut app = app(table);
        typed(&mut app, "example.org");
        assert_eq!(page_text(&app), "ab");
        assert_eq!(
            notes(&mut app),
            "3 of the page's style sheets were not applied"
        );
    }

    #[test]
    fn mixed_content_404s_and_wrong_types_are_noted_and_data_sheets_applied() {
        let table = Table::default()
            .page(
                "https://example.org/",
                "text/html",
                "<link rel=stylesheet href=/r.css><link rel=stylesheet href=/missing.css>\
                 <link rel=stylesheet href=/plain.css><link rel=stylesheet href=http://example.org/h.css>\
                 <link rel=stylesheet href='data:text/css,.d%7Bdisplay:none%7D'>\
                 <b class=r>r</b><b class=m>m</b><b class=p>p</b><b class=h>h</b><b class=d>d</b>",
            )
            .redirect("https://example.org/r.css", "http://example.org/r.css")
            .sheet("http://example.org/r.css", ".r{display:none}")
            .reply(
                "https://example.org/missing.css",
                404,
                &[("content-type", "text/css")],
                b".m{display:none}",
            )
            .page("https://example.org/plain.css", "text/plain", ".p{display:none}")
            .sheet("http://example.org/h.css", ".h{display:none}");
        let table = Arc::new(table);
        let mut app = app_over(table.clone());
        typed(&mut app, "example.org");
        assert_eq!(page_text(&app), "rmph");
        let urls = table.urls();
        assert!(
            !urls.iter().any(|url| url.starts_with("http://")),
            "no http hop is asked: {urls:?}"
        );
        assert!(!urls.iter().any(|url| url.starts_with("data:")));
        assert_eq!(
            notes(&mut app),
            "4 of the page's style sheets were not applied"
        );
    }

    #[test]
    fn a_sheet_past_style_wait_is_late_and_a_reload_asks_for_it_again() {
        let table = Table::default()
            .page(
                "https://example.org/",
                "text/html",
                "<link rel=stylesheet href=slow.css><b class=s>slow</b>",
            )
            .sheet("https://example.org/slow.css", ".s{display:none}");
        let (gate, release) = gate(table, &["https://example.org/slow.css"]);
        let mut app = app_over(gate.clone());
        app.style_wait = std::time::Duration::from_millis(150);
        type_address(&mut app, "example.org");
        // The page shown meanwhile is the one before, the status row
        // saying the styles are loading.
        let mut said = false;
        for _ in 0..1000 {
            app.poll(0);
            if app.status_text() == "loading styles of https://example.org/" {
                said = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(said, "{}", app.status_text());
        assert!(app.current.doc.url.is_none(), "the blank page stays");
        settle(&mut app);
        assert_eq!(page_text(&app), "slow");
        assert_eq!(
            notes(&mut app),
            "1 style sheet did not come in time; Ctrl+R asks again"
        );
        // Let go, the late one ends and frees its slot.
        release.send(()).unwrap();
        slots_free(&app);
        // A reload asks for it again; answered in time, it applies.
        release.send(()).unwrap();
        key(&mut app, "C-r");
        settle(&mut app);
        assert_eq!(page_text(&app), "");
        assert_eq!(notes(&mut app), "");
        let asked = started(&gate);
        assert_eq!(
            asked.iter().filter(|url| url.ends_with("slow.css")).count(),
            2
        );
    }

    #[test]
    fn stopped_fetches_hold_their_slots_until_the_service_answers() {
        let mut table = Table::default()
            .page(
                "https://example.org/b",
                "text/html",
                "<link rel=stylesheet href=/b.css><i class=b>b</i>",
            )
            .sheet("https://example.org/b.css", ".b{display:none}")
            .page("https://example.org/", "text/html", "<p>home");
        let mut markup = String::new();
        let mut held = Vec::new();
        for n in 0..subresource::MAX_SUBRESOURCES + 1 {
            let url = format!("https://example.org/a{n}.css");
            markup.push_str(&format!("<link rel=stylesheet href=/a{n}.css>"));
            table = table.sheet(&url, "");
            held.push(url);
        }
        let table = table.page("https://example.org/a", "text/html", &markup);
        let held: Vec<&str> = held.iter().map(String::as_str).collect();
        let (gate, release) = gate(table, &held);
        let mut app = app_over(gate.clone());
        typed(&mut app, "example.org");
        let used = app.renderer.budget().used();
        type_address(&mut app, "example.org/a");
        // Each slot's fetch asked of the service: one stopped before its
        // first hop would ask nothing and give its slot back.
        for n in 0..subresource::MAX_SUBRESOURCES {
            asked(&mut app, &gate, &format!("https://example.org/a{n}.css"));
        }
        assert_eq!(app.slots.out(), subresource::MAX_SUBRESOURCES);
        // Escape stops the page, which never shows; the page shown stays.
        key(&mut app, "Escape");
        assert_eq!(app.status_text(), "stopped");
        assert_eq!(page_text(&app), "home");
        idle(&app);
        // The fifth was never asked; the four stopped hold their slots.
        assert!(!started(&gate).iter().any(|url| url.ends_with("a4.css")));
        assert_eq!(app.slots.out(), subresource::MAX_SUBRESOURCES);
        // So the next page's sheet waits for a slot, past STYLE_WAIT.
        app.style_wait = std::time::Duration::from_millis(200);
        typed(&mut app, "example.org/b");
        assert_eq!(page_text(&app), "b");
        assert!(!started(&gate).iter().any(|url| url.ends_with("b.css")));
        assert_eq!(
            notes(&mut app),
            "1 style sheet did not come in time; Ctrl+R asks again"
        );
        // Once the service answers them, the slots free and the page
        // stopped has let everything go on the render thread.
        for _ in 0..subresource::MAX_SUBRESOURCES {
            release.send(()).unwrap();
        }
        slots_free(&app);
        key(&mut app, "C-r");
        settle(&mut app);
        assert_eq!(page_text(&app), "");
        // Back to the first page's charges: nothing of the stopped one
        // nor of the replaced ones is held.
        key(&mut app, "M-Left");
        settle(&mut app);
        app.renderer.idle();
        app.renderer.release();
        app.renderer.idle();
        assert_eq!(page_text(&app), "home");
        assert_eq!(app.renderer.budget().used(), used);
    }

    /// td-web's fetch service as its `[web-fetch]` unit runs it:
    /// `SERVICE_WORKERS` requests served at once, the rest waiting for a
    /// worker; an origin that never answers a `tarpit` URL, given up at
    /// the request's deadline, as the service gives it up, or never.
    struct Service {
        table: Table,
        tarpit: Vec<String>,
        workers: Mutex<usize>,
        free: std::sync::Condvar,
        /// Each request as it was served, and how long it waited for a
        /// worker.
        served: Mutex<Vec<(String, std::time::Duration)>>,
    }

    impl Fetcher for Service {
        fn get(&self, url: &str, ask: Ask) -> Result<Reply, String> {
            let came = std::time::Instant::now();
            let mut workers = self.workers.lock().unwrap();
            while *workers >= subresource::SERVICE_WORKERS {
                workers = self.free.wait(workers).unwrap();
            }
            *workers += 1;
            drop(workers);
            self.served
                .lock()
                .unwrap()
                .push((url.into(), came.elapsed()));
            let answer = if self.tarpit.iter().any(|tarpit| tarpit == url) {
                // A worker held to the deadline asked for, or for good.
                let until = ask
                    .deadline
                    .unwrap_or(came + std::time::Duration::from_secs(600));
                std::thread::sleep(until.saturating_duration_since(std::time::Instant::now()));
                Err("transport: the origin timed out".into())
            } else {
                self.table.get(url, ask)
            };
            *self.workers.lock().unwrap() -= 1;
            self.free.notify_one();
            answer
        }
    }

    #[test]
    fn a_page_of_tarpit_sheets_never_holds_a_navigation_back() {
        let mut markup = String::new();
        let mut tarpit = Vec::new();
        for n in 0..subresource::MAX_SUBRESOURCES + 2 {
            markup.push_str(&format!("<link rel=stylesheet href=/t{n}.css>"));
            tarpit.push(format!("https://example.org/t{n}.css"));
        }
        markup.push_str("<p>tarpit");
        let table = Table::default()
            .page("https://example.org/t", "text/html", &markup)
            .page("https://example.org/n", "text/html", "<p>next")
            .page(
                "https://example.org/s",
                "text/html",
                "<link rel=stylesheet href=/s.css><i class=s>styled</i>",
            )
            .sheet("https://example.org/s.css", ".s{display:none}");
        let service = Arc::new(Service {
            table,
            tarpit,
            workers: Mutex::new(0),
            free: std::sync::Condvar::new(),
            served: Mutex::new(Vec::new()),
        });
        let mut app = app_over(service.clone());
        app.style_wait = std::time::Duration::from_millis(1500);
        type_address(&mut app, "example.org/t");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let tarpits = |service: &Service| {
            let served = service.served.lock().unwrap();
            served
                .iter()
                .filter(|(url, _)| url.contains("/t") && url.ends_with(".css"))
                .count()
        };
        while tarpits(&service) < subresource::MAX_SUBRESOURCES {
            assert!(
                std::time::Instant::now() < deadline,
                "the sheets were not asked"
            );
            app.poll(0);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // Every subresource slot held by the tarpit: a navigation is
        // served at once, a worker its own.
        let started = std::time::Instant::now();
        typed(&mut app, "example.org/n");
        assert_eq!(page_text(&app), "next");
        let waited = service
            .served
            .lock()
            .unwrap()
            .iter()
            .find(|(url, _)| url.ends_with("/n"))
            .map(|(_, waited)| *waited)
            .unwrap();
        assert!(waited < std::time::Duration::from_millis(200), "{waited:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        // The tarpit's fetches end at its STYLE_WAIT, given up by the
        // service, and so free their slots and workers; the two it never
        // asked are never asked.
        slots_free(&app);
        let served = service.served.lock().unwrap().clone();
        let asked = served.iter().filter(|(url, _)| url.contains("/t")).count();
        assert_eq!(asked, 1 + subresource::MAX_SUBRESOURCES, "{served:?}");
        // So a page after it is styled within its own wait.
        typed(&mut app, "example.org/s");
        assert_eq!(page_text(&app), "");
        assert_eq!(notes(&mut app), "");
    }

    #[test]
    fn a_stopped_pages_sheet_asks_no_hop_after_the_one_out() {
        let page = "https://next-hop.example/r";
        let table = Table::default()
            .page(page, "text/html", "<link rel=stylesheet href=/r0.css><p>r")
            .redirect("https://next-hop.example/r0.css", "/r1.css")
            .sheet("https://next-hop.example/r1.css", "p{}")
            .page("https://next-hop.example/", "text/html", "<p>home");
        let (gate, release) = gate(table, &["https://next-hop.example/r0.css"]);
        let dropped = crate::document::tests::watch(page);
        let mut app = app_over(gate.clone());
        typed(&mut app, "next-hop.example");
        type_address(&mut app, "next-hop.example/r");
        asked(&mut app, &gate, "https://next-hop.example/r0.css");
        key(&mut app, "Escape");
        // Within a tick the page is let go and its fetch told.
        dropped
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        // The service answers the hop out with a redirect: not followed.
        release.send(()).unwrap();
        slots_free(&app);
        assert!(!started(&gate).iter().any(|url| url.ends_with("r1.css")));
        assert_eq!(page_text(&app), "home");
    }

    /// Waits for `gate` to have started `url`, polling the window.
    fn asked(app: &mut App, gate: &Gate, url: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !started(gate).iter().any(|started| started == url) {
            assert!(std::time::Instant::now() < deadline, "{url} not asked");
            app.poll(0);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn a_page_stopped_waiting_for_its_sheets_is_dropped_on_the_render_thread() {
        // Stopped while its one sheet is out: let go within a tick, not
        // when the sheet is answered.
        let page = "https://drop-waiting.example/";
        let table = Table::default()
            .page(page, "text/html", "<link rel=stylesheet href=/w.css><p>w")
            .sheet("https://drop-waiting.example/w.css", "p{}");
        let (gate, release) = gate(table, &["https://drop-waiting.example/w.css"]);
        let dropped = crate::document::tests::watch(page);
        let mut app = app_over(gate.clone());
        type_address(&mut app, "drop-waiting.example");
        asked(&mut app, &gate, "https://drop-waiting.example/w.css");
        let stopped = std::time::Instant::now();
        key(&mut app, "Escape");
        let name = dropped
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert_eq!(name.as_deref(), Some("td-web render"));
        assert!(
            stopped.elapsed() < subresource::TICK * 25,
            "{:?}",
            stopped.elapsed()
        );
        release.send(()).unwrap();
        slots_free(&app);
        // Stopped between its jobs: one sheet decoded into it, the other
        // still out.
        let page = "https://drop-between.example/";
        let table = Table::default()
            .page(
                page,
                "text/html",
                "<link rel=stylesheet href=/a.css><link rel=stylesheet href=/b.css><p>v",
            )
            .sheet("https://drop-between.example/a.css", "a{}")
            .sheet("https://drop-between.example/b.css", "b{}");
        let (gate, release) = super::tests::gate(table, &["https://drop-between.example/b.css"]);
        let dropped = crate::document::tests::watch(page);
        let mut app = app_over(gate.clone());
        type_address(&mut app, "drop-between.example");
        asked(&mut app, &gate, "https://drop-between.example/b.css");
        // a's slot is given back once the render thread has taken it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while app.slots.out() > 1 {
            assert!(std::time::Instant::now() < deadline);
            app.poll(0);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        key(&mut app, "Escape");
        let name = dropped
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert_eq!(name.as_deref(), Some("td-web render"));
        release.send(()).unwrap();
        slots_free(&app);
    }

    /// Sheets refused at once, as a page on a LAN device's are, faster
    /// than the page takes their answers: none is lost, so the page is
    /// styled as soon as they settle, never at its wait's end, and a good
    /// sheet among them is applied.
    #[test]
    fn sheets_refused_at_once_are_each_answered() {
        let mut table = Table::default();
        let mut links = String::new();
        for n in 0..crate::gather::MAX_SHEET_FETCHES {
            links.push_str(&format!("<link rel=stylesheet href=/f{n}.css>"));
        }
        for n in 0..10 {
            table = table.page(&format!("https://example.org/p{n}"), "text/html", &links);
        }
        let mut around = String::new();
        for n in 0..10 {
            around.push_str(&format!("<link rel=stylesheet href=/g{n}.css>"));
        }
        let good = format!(
            "{around}<link rel=stylesheet href=/good.css>{}<i class=x>hidden</i>",
            around.replace("/g", "/h")
        );
        for n in 0..10 {
            table = table.page(&format!("https://example.org/q{n}"), "text/html", &good);
        }
        let table = table.sheet("https://example.org/good.css", ".x{display:none}");
        // `settle` gives up long before the minute's wait would end.
        let mut app = app_over(Arc::new(table));
        for n in 0..10 {
            typed(&mut app, &format!("example.org/p{n}"));
            assert_eq!(
                notes(&mut app),
                "32 of the page's style sheets were not applied",
                "page {n}"
            );
        }
        for n in 0..10 {
            typed(&mut app, &format!("example.org/q{n}"));
            assert_eq!(page_text(&app), "", "page {n}");
            assert_eq!(
                notes(&mut app),
                "20 of the page's style sheets were not applied",
                "page {n}"
            );
        }
    }

    /// A PNG of `width` by `height` pixels, each `rgb`.
    fn png(width: usize, height: usize, rgb: [u8; 3]) -> Vec<u8> {
        td_png::encode_rgb8(width, height, &rgb.repeat(width * height)).unwrap()
    }

    /// Polls until `done`, within a long wait: none of these is about
    /// lateness.
    fn until(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !done(app) {
            assert!(std::time::Instant::now() < deadline, "{what}");
            app.poll(0);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Waits for the page's pictures to come and be laid.
    fn pictured(app: &mut App) {
        until(app, "the pictures did not come", |app| {
            !app.current.pictures.as_ref().is_some_and(Pictures::busy)
                && app.current.relayout.is_none()
                && !app.current.again
        });
    }

    /// The pictures the layout shown lays: each node's index and size.
    fn fits(app: &App) -> Vec<(usize, usize, usize)> {
        crate::page::tests::fits(laid(app))
    }

    /// The node index of each picture the page names, in tree order.
    fn picture_nodes(app: &App) -> Vec<usize> {
        laid(app)
            .tree()
            .pictures()
            .iter()
            .map(|node| node.index())
            .collect()
    }

    fn refused(app: &App, at: usize) -> Option<pictures::Why> {
        app.current.pictures.as_ref().unwrap().refused(at)
    }

    /// The glyphs a paint of the page draws, in order.
    fn glyph_text(app: &mut App) -> String {
        painted(app).iter().map(|&(_, c, _)| c).collect()
    }

    /// The pictures a paint of the page draws: each one's id and box.
    fn images(app: &mut App) -> Vec<(u64, Rect)> {
        let pane = app.pane();
        let frame = app.current.view.frame(&pane);
        let marks = Marks {
            hits: &[],
            selection: None,
            link: None,
        };
        let mut scratch = Scratch::new(app.renderer.budget());
        let (laid, measure) =
            measure_of(&app.current.doc, &mut app.current.measure, &app.renderer).unwrap();
        let mut out = Vec::new();
        paint::paint(
            &laid,
            measure,
            &frame,
            app.surface.bounds(),
            &marks,
            &mut scratch,
            &mut |draw| {
                if let Primitive::Image { id, rect } = draw.primitive {
                    out.push((id, rect));
                }
            },
        );
        out
    }

    /// The middle of the box the node of index `node` is laid in, in the
    /// window.
    fn picture_point(app: &App, node: usize) -> (i64, i64) {
        let pane = app.pane();
        let frame = app.current.view.frame(&pane);
        let scale = frame.scale as f64;
        let laid = laid(app);
        let inline = laid
            .lines()
            .iter()
            .flat_map(|line| line.replaced.iter())
            .find(|r| r.node.index() == node)
            .map(|r| (r.x, r.y, r.width, r.height));
        // Else a block-level one, its content box.
        let (x, y, width, height) = inline.unwrap_or_else(|| {
            let id = laid
                .tree()
                .pictures()
                .iter()
                .find(|id| id.index() == node)
                .copied()
                .unwrap();
            let g = laid.geometry(laid.geometry_of(id).unwrap()).unwrap();
            assert!(g.replaced);
            (g.content.x, g.content.y, g.content.width, g.content.height)
        });
        (
            frame.x + ((x + width / 2.0) * scale) as i64,
            frame.y + ((y + height / 2.0) * scale) as i64,
        )
    }

    /// The window painted: its pixels, BGRA.
    fn window_pixels(app: &mut App) -> Vec<u8> {
        let font = td_ui::font::pinned().unwrap();
        let surface = app.surface;
        let mut pixels = vec![0u8; surface.width * surface.height * 4];
        let mut raster = Raster::new(&mut pixels, &font, surface, surface.width * 4).unwrap();
        app.paint(&mut raster, surface).unwrap();
        drop(raster);
        pixels
    }

    fn pixel(app: &App, pixels: &[u8], (x, y): (i64, i64)) -> [u8; 4] {
        let at = (y as usize * app.surface.width + x as usize) * 4;
        pixels[at..at + 4].try_into().unwrap()
    }

    /// The URLs asked for that end in `tail`, and what each asked.
    fn asks(table: &Table, tail: &str) -> Vec<(String, Ask)> {
        let asked = table.asked.lock().unwrap();
        asked
            .iter()
            .filter(|(url, _)| url.ends_with(tail))
            .cloned()
            .collect()
    }

    #[test]
    fn a_pages_pictures_come_in_place_of_their_placeholders() {
        let table = Arc::new(
            Table::default()
                .page(
                    "https://example.org/p",
                    "text/html",
                    "<body style=margin:0><p>top<p><img src=/a.png alt=qqq> \
                     <img src=' b.png ' alt=zzz> <img src=a.png width=15 alt=w>",
                )
                .picture("https://example.org/a.png", &png(30, 20, [255, 0, 0]))
                .picture("https://example.org/b.png", &png(10, 40, [0, 0, 255])),
        );
        let mut app = app_over(table.clone());
        typed(&mut app, "example.org/p");
        // Shown first, before any has come, with its placeholders.
        assert!(fits(&app).is_empty());
        let glyphs = glyph_text(&mut app);
        assert!(
            glyphs.contains("qqq") && glyphs.contains("zzz") && glyphs.ends_with("w"),
            "{glyphs}"
        );
        assert!(images(&mut app).is_empty());
        pictured(&mut app);
        let nodes = picture_nodes(&app);
        assert_eq!(
            fits(&app),
            vec![(nodes[0], 30, 20), (nodes[1], 10, 40), (nodes[2], 15, 10)]
        );
        // At their own size drawn with no copy; one the page sizes, a
        // copy fitted to its box.
        assert_eq!(crate::page::tests::scaled(laid(&app), nodes[0]), Some(1));
        assert_eq!(crate::page::tests::scaled(laid(&app), nodes[2]), None);
        // Each drawn in its box in place of its alt text.
        assert_eq!(glyph_text(&mut app), "top");
        assert_eq!(images(&mut app).len(), 3);
        let pixels = window_pixels(&mut app);
        assert_eq!(
            pixel(&app, &pixels, picture_point(&app, nodes[0])),
            [0, 0, 255, 255]
        );
        assert_eq!(
            pixel(&app, &pixels, picture_point(&app, nodes[1])),
            [255, 0, 0, 255]
        );
        assert_eq!(
            pixel(&app, &pixels, picture_point(&app, nodes[2])),
            [0, 0, 255, 255]
        );
        assert_eq!(notes(&mut app), "");
        // Each asked public, as a picture asks, within its wait.
        let asked = asks(&table, ".png");
        let mut urls: Vec<_> = asked.iter().map(|(url, _)| url.as_str()).collect();
        urls.sort_unstable();
        // Each `img` its own fetch: td-web keeps no cache.
        assert_eq!(
            urls,
            [
                "https://example.org/a.png",
                "https://example.org/a.png",
                "https://example.org/b.png"
            ]
        );
        for (_, ask) in &asked {
            assert!(ask.public);
            assert_eq!(ask.accept, pictures::ACCEPT);
            assert_eq!(ask.limit, pictures::MAX_PICTURE_BODY);
            let left = ask.deadline.unwrap() - std::time::Instant::now();
            assert!(left <= pictures::PICTURE_WAIT, "{left:?}");
        }
        slots_free(&app);
    }

    /// At scale two a picture laid at its size fills a box twice its
    /// pixels each way, drawn scaled with no copy made.
    #[test]
    fn a_picture_at_scale_two_fills_its_box_with_no_copy() {
        let table = Table::default()
            .page(
                "https://example.org/two",
                "text/html",
                "<body style=margin:0><img src=/a.png>",
            )
            .picture("https://example.org/a.png", &png(3, 2, [255, 0, 0]));
        let surface = Surface::new(800, 600, Scale::new(2).unwrap()).unwrap();
        let mut app = App::new(Arc::new(table), surface, None).unwrap();
        app.style_wait = std::time::Duration::from_secs(60);
        typed(&mut app, "example.org/two");
        pictured(&mut app);
        let nodes = picture_nodes(&app);
        assert_eq!(fits(&app), vec![(nodes[0], 6, 4)]);
        assert_eq!(crate::page::tests::scaled(laid(&app), nodes[0]), Some(2));
        let pixels = window_pixels(&mut app);
        // The box's top left, from its middle.
        let (x, y) = picture_point(&app, nodes[0]);
        let (left, top) = (x - 3, y - 2);
        for (x, y) in [(0, 0), (5, 3), (2, 1)] {
            assert_eq!(
                pixel(&app, &pixels, (left + x, top + y)),
                [0, 0, 255, 255],
                "at {x}, {y}"
            );
        }
        assert_ne!(pixel(&app, &pixels, (left + 6, top)), [0, 0, 255, 255]);
        assert_ne!(pixel(&app, &pixels, (left, top + 4)), [0, 0, 255, 255]);
        slots_free(&app);
    }

    #[test]
    fn a_refused_picture_keeps_its_placeholder_and_is_noted() {
        let good = png(4, 4, [0, 255, 0]);
        let table = Arc::new(
            Table::default()
                .page(
                    "https://example.org/r",
                    "text/html",
                    "<img src=/gone.png alt=a0><img src=/e404.png alt=a1>\
                     <img src=/x.png alt=a2><img src=/bad.png alt=a3>\
                     <img src=/s.svg alt=a4><img src=http://example.org/m.png alt=a5>\
                     <img src=/ok.gif alt=a6><img src=mailto:x@example.org alt=a7>\
                     <img src='  ' alt=a8>",
                )
                .reply(
                    "https://example.org/e404.png",
                    404,
                    &[("content-type", "image/png")],
                    &good,
                )
                .reply(
                    "https://example.org/x.png",
                    200,
                    &[("content-type", "image/svg+xml")],
                    &good,
                )
                .reply(
                    "https://example.org/bad.png",
                    200,
                    &[("content-type", "image/png")],
                    b"not a picture",
                )
                // Sniffed: what it is, whatever its type says.
                .reply(
                    "https://example.org/ok.gif",
                    200,
                    &[("content-type", "image/gif")],
                    &good,
                )
                .picture("https://example.org/s.svg", &good)
                .picture("http://example.org/m.png", &good),
        );
        let mut app = app_over(table.clone());
        typed(&mut app, "example.org/r");
        pictured(&mut app);
        // The blank `src` names no picture.
        let nodes = picture_nodes(&app);
        assert_eq!(nodes.len(), 8);
        assert_eq!(fits(&app), vec![(nodes[6], 4, 4)]);
        use pictures::Why::{Failed, Skipped};
        let why: Vec<_> = (0..8).map(|at| refused(&app, at)).collect();
        assert_eq!(
            why,
            [
                Some(Failed),
                Some(Failed),
                Some(Failed),
                Some(Failed),
                Some(Skipped),
                Some(Failed),
                None,
                Some(Skipped)
            ]
        );
        assert_eq!(notes(&mut app), "7 of the page's 8 pictures are not shown");
        // SVG and mixed content are never asked.
        let urls = table.urls();
        assert!(!urls.iter().any(|url| url.ends_with(".svg")), "{urls:?}");
        assert!(!urls.iter().any(|url| url.starts_with("http:")), "{urls:?}");
        // Each refused keeps its placeholder and alt text.
        let glyphs = glyph_text(&mut app);
        assert_eq!(glyphs, "a0a1a2a3a4a5a7a8");
        slots_free(&app);
    }

    #[test]
    fn a_picture_redirected_to_http_is_refused_at_that_hop() {
        let picture = png(3, 3, [9, 9, 9]);
        let table = Arc::new(
            Table::default()
                .page(
                    "https://example.org/m",
                    "text/html",
                    "<img src=/r.png alt=r>",
                )
                .redirect("https://example.org/r.png", "http://example.org/r.png")
                .picture("http://example.org/r.png", &picture)
                .page(
                    "http://plain.example/",
                    "text/html",
                    "<img src=/p.png alt=p>",
                )
                .picture("http://plain.example/p.png", &picture),
        );
        let mut app = app_over(table.clone());
        typed(&mut app, "example.org/m");
        pictured(&mut app);
        assert!(fits(&app).is_empty());
        assert_eq!(refused(&app, 0), Some(pictures::Why::Failed));
        assert_eq!(notes(&mut app), "the page's picture is not shown");
        let urls = table.urls();
        assert!(urls.contains(&"https://example.org/r.png".to_string()));
        assert!(!urls.contains(&"http://example.org/r.png".to_string()));
        // An `http` page's `http` picture is no mixed content.
        typed(&mut app, "http://plain.example/");
        pictured(&mut app);
        assert_eq!(fits(&app).len(), 1);
        slots_free(&app);
    }

    #[test]
    fn pictures_past_max_pictures_keep_their_placeholders() {
        let count = pictures::MAX_PICTURES + 2;
        let picture = png(1, 1, [1, 1, 1]);
        let mut table = Table::default().page(
            "https://example.org/many",
            "text/html",
            &(0..count)
                .map(|n| format!("<img src=/p{n}.png alt=p>"))
                .collect::<String>(),
        );
        for n in 0..count {
            table = table.picture(&format!("https://example.org/p{n}.png"), &picture);
        }
        let table = Arc::new(table);
        let mut app = app_over(table.clone());
        typed(&mut app, "example.org/many");
        pictured(&mut app);
        assert_eq!(fits(&app).len(), pictures::MAX_PICTURES);
        // The first in tree order, each once.
        let mut asked = table.urls();
        asked.retain(|url| url.ends_with(".png"));
        asked.sort_unstable();
        let mut first: Vec<_> = (0..pictures::MAX_PICTURES)
            .map(|n| format!("https://example.org/p{n}.png"))
            .collect();
        first.sort_unstable();
        assert_eq!(asked, first);
        assert_eq!(
            notes(&mut app),
            format!("2 of the page's {count} pictures are not shown")
        );
        slots_free(&app);
    }

    /// The pictures term refuses a decoding past it, and a fitted copy;
    /// each keeps its placeholder.
    #[test]
    fn the_pictures_term_bounds_what_a_page_decodes_and_fits() {
        let table = Table::default()
            .page(
                "https://example.org/t",
                "text/html",
                "<img src=/small.png alt=s><img src=/big.png alt=b>\
                 <img src=/small.png width=1100 height=1000 alt=w>",
            )
            .picture("https://example.org/small.png", &png(4, 4, [5, 5, 5]))
            .picture("https://example.org/big.png", &png(1100, 1000, [5, 5, 5]));
        let mut app = app(table);
        // Room for the small one and its bookkeeping; 1100 by 1000 pixels
        // are 4.2 MiB.
        app.pictures_term = 4 << 20;
        typed(&mut app, "example.org/t");
        pictured(&mut app);
        let nodes = picture_nodes(&app);
        assert_eq!(fits(&app), vec![(nodes[0], 4, 4)]);
        assert_eq!(refused(&app, 1), Some(pictures::Why::Room));
        // Decoded, but its box takes more than the term has.
        assert_eq!(refused(&app, 2), None);
        assert_eq!(glyph_text(&mut app), "bw");
        assert_eq!(
            app.current.pictures.as_ref().unwrap().term().limit(),
            (4 << 20) + td_alloc::Budget::metadata_bytes()
        );
        slots_free(&app);
    }

    /// Pictures that come within `PICTURE_SETTLE` of a settle wait for the
    /// next: those come are laid together by one relayout, the reader's
    /// place kept, and a fitted copy at the same size kept.
    #[test]
    fn pictures_come_together_are_laid_by_one_relayout_keeping_the_top() {
        let markup = format!(
            "<img src=/a.png width=10 alt=a><img src=/b.png alt=b><img src=/c.png alt=c>{}",
            numbered(200)
        );
        let table = Table::default()
            .page("https://example.org/s", "text/html", &markup)
            .picture("https://example.org/a.png", &png(20, 100, [1, 2, 3]))
            .picture("https://example.org/b.png", &png(20, 300, [1, 2, 3]))
            .picture("https://example.org/c.png", &png(20, 200, [1, 2, 3]));
        let (gate, release) = gate(
            table,
            &["https://example.org/b.png", "https://example.org/c.png"],
        );
        let mut app = app_over(gate.clone());
        app.clock.fixed = Some(1_000);
        typed(&mut app, "example.org/s");
        // The first comes and is laid at once: no settle before it.
        until(&mut app, "a was not laid", |app| {
            fits(app).len() == 1 && app.current.relayout.is_none()
        });
        let nodes = picture_nodes(&app);
        for _ in 0..5 {
            key(&mut app, "PageDown");
        }
        let before = top(&app);
        let text = line_text(&app, top_line(&app));
        let kept = crate::page::tests::fit_copy(laid(&app), nodes[0])
            .try_clone()
            .unwrap();
        // b and c come within PICTURE_SETTLE of it: taken, not laid.
        release.send(()).unwrap();
        release.send(()).unwrap();
        until(&mut app, "b and c did not come", |app| {
            pictures::tests::came(app.current.pictures.as_ref().unwrap()) == 3
        });
        for _ in 0..10 {
            app.poll(0);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(app.current.relayout.is_none());
        assert_eq!(fits(&app).len(), 1);
        assert!(notes(&mut app).ends_with(" · loading pictures, 1 of 3 shown"));
        // A relayout for another cause lays those settled only.
        key(&mut app, "F8");
        relaid(&mut app);
        assert_eq!(fits(&app).len(), 1);
        // At PICTURE_SETTLE, one relayout lays both.
        app.clock.fixed = Some(1_000 + pictures::PICTURE_SETTLE);
        app.poll(0);
        assert!(app.current.relayout.is_some());
        relaid(&mut app);
        assert_eq!(fits(&app).len(), 3);
        assert!(!app.current.pictures.as_ref().unwrap().busy());
        assert_eq!(top(&app).place, before.place);
        assert_eq!(line_text(&app, top_line(&app)), text);
        assert!(BudgetShared::ptr_eq(
            &kept,
            crate::page::tests::fit_copy(laid(&app), nodes[0])
        ));
        drop(kept);
        app.poll(0);
        assert!(app.current.relayout.is_none() && !app.current.again);
        assert!(!notes(&mut app).contains("picture"));
        slots_free(&app);
    }

    #[test]
    fn a_click_on_an_unlinked_picture_opens_it() {
        let picture = png(40, 30, [7, 7, 7]);
        let table = site()
            .page(
                "https://example.org/u",
                "text/html",
                "<img src=/u.png alt=u> <a href=/b><img src=/l.png alt=l></a>",
            )
            .picture("https://example.org/u.png", &picture)
            .picture("https://example.org/l.png", &picture);
        let mut app = app(table);
        typed(&mut app, "example.org/u");
        pictured(&mut app);
        let nodes = picture_nodes(&app);
        assert_eq!(fits(&app).len(), 2);
        let unlinked = picture_point(&app, nodes[0]);
        app.mouse(PointerPhase::Press, unlinked, false);
        app.mouse(PointerPhase::Release, unlinked, false);
        settle(&mut app);
        assert_eq!(app.title(), "https://example.org/u.png - td-web");
        key(&mut app, "M-Left");
        settle(&mut app);
        pictured(&mut app);
        let linked = picture_point(&app, nodes[1]);
        app.mouse(PointerPhase::Press, linked, false);
        app.mouse(PointerPhase::Release, linked, false);
        settle(&mut app);
        assert_eq!(app.title(), "B - td-web");
    }

    #[test]
    fn escape_stops_the_pictures_still_to_come_and_a_reload_asks_again() {
        let h = "https://example.org/h.png";
        let table = Table::default()
            .page(
                "https://example.org/e",
                "text/html",
                "<img src=/ok.png alt=k><img src=/h.png alt=h>",
            )
            .picture("https://example.org/ok.png", &png(5, 5, [3, 3, 3]))
            .picture(h, &png(6, 6, [3, 3, 3]));
        let (gate, release) = gate(table, &[h]);
        let mut app = app_over(gate.clone());
        typed(&mut app, "example.org/e");
        asked(&mut app, &gate, h);
        until(&mut app, "ok was not laid", |app| {
            fits(app).len() == 1 && app.current.relayout.is_none()
        });
        assert_eq!(notes(&mut app), "loading pictures, 1 of 2 shown");
        key(&mut app, "Escape");
        assert_eq!(app.status_text(), "pictures stopped");
        assert_eq!(
            notes(&mut app),
            "1 of the page's 2 pictures is not shown; Ctrl+R asks again"
        );
        assert_eq!(refused(&app, 1), Some(pictures::Why::Stopped));
        // Answered after, it is let go, never shown.
        release.send(()).unwrap();
        slots_free(&app);
        for _ in 0..10 {
            app.poll(0);
        }
        assert_eq!(fits(&app).len(), 1);
        assert_eq!(refused(&app, 1), Some(pictures::Why::Stopped));
        // Nothing more to stop.
        key(&mut app, "Escape");
        assert_ne!(app.status_text(), "pictures stopped");
        // A reload asks again.
        key(&mut app, "C-r");
        settle(&mut app);
        until(&mut app, "h was not asked again", |_| {
            started(&gate).iter().filter(|url| *url == h).count() == 2
        });
        release.send(()).unwrap();
        pictured(&mut app);
        assert_eq!(fits(&app).len(), 2);
        assert_eq!(notes(&mut app), "");
        slots_free(&app);
    }

    /// Escape stops a navigation out before the pictures of the page
    /// shown, which wait for it to end before asking for more.
    #[test]
    fn escape_stops_a_navigation_before_the_pictures() {
        let h = "https://example.org/h.png";
        let next = "https://example.org/next";
        let table = Table::default()
            .page(
                "https://example.org/e",
                "text/html",
                "<img src=/h.png><img src=/i.png><img src=/j.png>",
            )
            .page(next, "text/html", "<p>next")
            .picture(h, &png(2, 2, [3, 3, 3]))
            .picture("https://example.org/i.png", &png(2, 2, [3, 3, 3]))
            .picture("https://example.org/j.png", &png(2, 2, [3, 3, 3]));
        let i = "https://example.org/i.png";
        let (gate, release, apart) = gate_apart(table, &[h, next], &[i]);
        let mut app = app_over(gate.clone());
        typed(&mut app, "example.org/e");
        asked(&mut app, &gate, h);
        asked(&mut app, &gate, i);
        type_address(&mut app, "example.org/next");
        asked(&mut app, &gate, next);
        // While the navigation is out, i's answer is taken but j, whose
        // slot it frees, is not asked.
        apart[0].send(()).unwrap();
        until(&mut app, "i did not come", |app| {
            pictures::tests::came(app.current.pictures.as_ref().unwrap()) == 1
        });
        for _ in 0..10 {
            app.poll(0);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(!started(&gate).iter().any(|url| url.ends_with("/j.png")));
        key(&mut app, "Escape");
        assert_eq!(app.status_text(), "stopped");
        assert!(app.current.pictures.as_ref().unwrap().busy());
        key(&mut app, "Escape");
        assert_eq!(app.status_text(), "pictures stopped");
        release.send(()).unwrap();
        release.send(()).unwrap();
        slots_free(&app);
        idle(&app);
    }

    /// A page overtaken while its pictures come lets them go on the render
    /// thread, its decoded pictures and fitted copies with them, and
    /// returns every charge.
    #[test]
    fn an_overtaken_pages_pictures_go_on_the_render_thread() {
        let page = "https://drops.example/";
        let h = "https://drops.example/h.png";
        let table = Table::default()
            .page("https://drops.example/home", "text/html", "<p>home")
            .page(
                page,
                "text/html",
                "<img src=/a.png width=5><img src=/h.png>",
            )
            .picture("https://drops.example/a.png", &png(8, 8, [1, 2, 3]))
            .picture(h, &png(8, 8, [1, 2, 3]));
        let (gate, release) = gate(table, &[h]);
        let told = pictures::tests::watch(page);
        let mut app = app_over(gate.clone());
        typed(&mut app, "drops.example/home");
        app.renderer.idle();
        app.renderer.release();
        app.renderer.idle();
        let used = app.renderer.budget().used();
        typed(&mut app, "drops.example/");
        asked(&mut app, &gate, h);
        until(&mut app, "a was not laid", |app| {
            fits(app).len() == 1 && app.current.relayout.is_none()
        });
        typed(&mut app, "drops.example/home");
        // h answers after: decoded by no one.
        release.send(()).unwrap();
        slots_free(&app);
        idle(&app);
        app.renderer.idle();
        app.renderer.release();
        app.renderer.idle();
        let mut drops: Vec<_> = told.try_iter().collect();
        drops.sort_unstable();
        assert_eq!(drops, [("fit", true), ("inline", true), ("pictures", true)]);
        assert_eq!(app.renderer.budget().used(), used);
    }

    /// Pictures take at most `PICTURE_SLOTS` of the subresource slots:
    /// a page of pictures whose origin never answers holds back neither a
    /// navigation nor the next page's sheets.
    #[test]
    fn a_page_of_tarpit_pictures_holds_back_no_navigation_or_sheet() {
        let mut markup = String::new();
        let mut tarpit = Vec::new();
        for n in 0..6 {
            markup.push_str(&format!("<img src=/t{n}.png>"));
            tarpit.push(format!("https://example.org/t{n}.png"));
        }
        let table = Table::default()
            .page("https://example.org/t", "text/html", &markup)
            .page(
                "https://example.org/s",
                "text/html",
                "<link rel=stylesheet href=/s.css><i class=s>styled</i>",
            )
            .sheet("https://example.org/s.css", ".s{display:none}");
        let service = Arc::new(Service {
            table,
            tarpit,
            workers: Mutex::new(0),
            free: std::sync::Condvar::new(),
            served: Mutex::new(Vec::new()),
        });
        let mut app = app_over(service.clone());
        // Long past the bound below, so a loaded host's slow navigation
        // is not taken for one held back.
        app.picture_wait = std::time::Duration::from_secs(20);
        typed(&mut app, "example.org/t");
        let tarpits = |service: &Service| {
            let served = service.served.lock().unwrap();
            served
                .iter()
                .filter(|(url, _)| url.ends_with(".png"))
                .count()
        };
        until(&mut app, "the pictures were not asked", |_| {
            tarpits(&service) == pictures::PICTURE_SLOTS
        });
        for _ in 0..20 {
            app.poll(0);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(tarpits(&service), pictures::PICTURE_SLOTS);
        assert_eq!(app.slots.out(), pictures::PICTURE_SLOTS);
        // The next page's sheet takes a slot the pictures left.
        let started = std::time::Instant::now();
        typed(&mut app, "example.org/s");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(page_text(&app), "");
        assert_eq!(notes(&mut app), "");
        // The tarpit's fetches end at their wait; none past them asked.
        until(&mut app, "the tarpit's fetches did not end", |app| {
            app.slots.out() == 0
        });
        assert_eq!(tarpits(&service), pictures::PICTURE_SLOTS);
    }

    #[test]
    fn a_data_picture_is_decoded_with_no_request() {
        let picture = png(6, 2, [0, 255, 0]);
        let markup = format!(
            "<img src=\"data:image/png;base64,{}\" alt=d><img src=\"data:,nope\" alt=n>",
            base64(&picture)
        );
        let table = Arc::new(Table::default().page("https://example.org/d", "text/html", &markup));
        let mut app = app_over(table.clone());
        typed(&mut app, "example.org/d");
        pictured(&mut app);
        let nodes = picture_nodes(&app);
        assert_eq!(fits(&app), vec![(nodes[0], 6, 2)]);
        assert_eq!(refused(&app, 1), Some(pictures::Why::Failed));
        assert_eq!(table.urls(), ["https://example.org/d"]);
    }

    /// The render thread held on a relayout of its own until the guard
    /// goes: what is posted after it waits in the lane.
    fn hold_render(app: &App) -> impl Drop {
        use crate::document::tests::{again_of, hold, WAITS};
        let held = hold();
        let page = app.current.doc.page().unwrap();
        let want = Want {
            width: WAITS,
            ..app.want()
        };
        let again = again_of(page, want);
        let (faces, cancel, answer) = app.renderer.relayout_parts().unwrap();
        assert!(app
            .renderer
            .post(Lane::Again {
                again,
                faces,
                cancel,
                answer,
            })
            .is_ok());
        std::thread::sleep(std::time::Duration::from_millis(50));
        held
    }

    /// A page of `count` `data:` pictures one pixel wide and 8192 tall,
    /// each laid 8192 wide and one tall.
    fn thin_pictures(count: usize) -> String {
        let picture = png(1, 8192, [9, 99, 199]);
        let src = format!("data:image/png;base64,{}", base64(&picture));
        let mut markup = String::from("<body style=margin:0><p>thin");
        for _ in 0..count {
            markup.push_str(&format!("<img src=\"{src}\" width=8192 height=1>"));
        }
        markup.push_str("<p>end");
        markup
    }

    /// The fitted copies one layout makes take at most its work: one
    /// past it keeps its placeholder, is not counted shown, and is noted.
    /// Both come before the window takes either, so one settle lays both.
    #[test]
    fn a_layouts_copies_take_at_most_its_work() {
        let (a, b) = ("https://example.org/a.png", "https://example.org/b.png");
        let table = Table::default()
            .page(
                "https://example.org/w",
                "text/html",
                "<body style=margin:0><img src=/a.png width=15 height=15 alt=aa>\
                 <img src=/b.png width=15 height=15 alt=bb>",
            )
            .picture(a, &png(10, 10, [0, 0, 255]))
            .picture(b, &png(10, 10, [0, 0, 255]));
        let (gate, release) = gate(table, &[a, b]);
        let mut app = app_over(gate.clone());
        let one = td_ui::picture::Picture::new(10, 10, vec![0; 400])
            .unwrap()
            .into_fitted()
            .fit_exact_cost(15, 15)
            .unwrap()
            .steps;
        app.fit_work = one + one / 2;
        typed(&mut app, "example.org/w");
        asked(&mut app, &gate, a);
        asked(&mut app, &gate, b);
        relaid(&mut app);
        let held = hold_render(&app);
        release.send(()).unwrap();
        release.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        drop(held);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while pictures::tests::ready(app.current.pictures.as_ref().unwrap()) < 2 {
            assert!(std::time::Instant::now() < deadline, "not decoded");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        pictured(&mut app);
        let nodes = picture_nodes(&app);
        assert_eq!(fits(&app), vec![(nodes[0], 15, 15)]);
        assert_eq!(laid(&app).short(), 1);
        assert_eq!(glyph_text(&mut app), "bb");
        assert_eq!(notes(&mut app), "1 of the page's 2 pictures is not shown");
        slots_free(&app);
    }

    /// A page of thin pictures laid very wide (DESIGN.md §10's bound on
    /// fitting): each copy costs its axes in turn, so all are laid in a
    /// relayout of bounded work, and a navigation from it cancels the
    /// relayout it starts beside, so its own page is made first; the page
    /// is laid again once the navigation ends without a page.
    #[test]
    fn a_navigation_cancels_the_relayout_of_the_page_shown() {
        let next = "https://example.org/next";
        let table = Table::default()
            .page(
                "https://example.org/thin",
                "text/html",
                &thin_pictures(pictures::MAX_PICTURES),
            )
            .page(next, "text/html", "<p>next");
        let (gate, release) = gate(table, &[next]);
        let mut app = app_over(gate.clone());
        typed(&mut app, "example.org/thin");
        pictured(&mut app);
        assert_eq!(fits(&app).len(), pictures::MAX_PICTURES);
        assert_eq!(laid(&app).short(), 0);
        let fitted = std::time::Instant::now();
        resize(&mut app, 700);
        let laid_again = fitted.elapsed();
        assert_eq!(fits(&app).len(), pictures::MAX_PICTURES);
        // A relayout asked, then a navigation before it is taken.
        app.input(Input::Resize(surface(760)), &mut Board::default());
        let running = app
            .current
            .relayout
            .as_ref()
            .unwrap()
            .cancel
            .try_clone()
            .unwrap();
        let started = std::time::Instant::now();
        type_address(&mut app, "example.org/next");
        assert!(running.load(Ordering::SeqCst));
        assert!(app.current.relay_after);
        asked(&mut app, &gate, next);
        // Cancelled, it is not asked again while the navigation is out.
        until(&mut app, "the relayout was not taken", |app| {
            app.current.relayout.is_none()
        });
        assert!(app.current.again);
        // Stopped: the page shown is laid again for the window.
        key(&mut app, "Escape");
        relaid(&mut app);
        assert!(!app.current.relay_after);
        assert_eq!(laid(&app).want.width, app.want().width);
        assert_eq!(fits(&app).len(), pictures::MAX_PICTURES);
        // Asked again, its page made behind no relayout.
        app.input(Input::Resize(surface(700)), &mut Board::default());
        type_address(&mut app, "example.org/next");
        // The stopped fetch, then this one.
        release.send(()).unwrap();
        release.send(()).unwrap();
        settle(&mut app);
        let shown = started.elapsed();
        assert_eq!(page_text(&app), "next");
        eprintln!(
            "thin pictures: laid again in {laid_again:.1?}, the navigation shown in {shown:.1?}"
        );
        slots_free(&app);
    }

    /// Block-level and floated pictures, and one in a link, are laid and
    /// drawn as inline ones are; a refused one draws its frame and alt.
    #[test]
    fn block_level_pictures_are_laid_and_drawn() {
        let table = site()
            .page(
                "https://example.org/blocks",
                "text/html",
                "<body style=margin:0><img src=/a.png style=display:block alt=a>\
                 <img src=/b.png style=float:left width=20 alt=b>\
                 <a href=/x><img src=/c.png style=display:block alt=c></a>\
                 <img src=/gone.png style=display:block width=60 height=30 alt=zz>",
            )
            .picture("https://example.org/a.png", &png(30, 20, [255, 0, 0]))
            .picture("https://example.org/b.png", &png(10, 10, [0, 255, 0]))
            .picture("https://example.org/c.png", &png(12, 8, [0, 0, 255]));
        let mut app = app(table);
        typed(&mut app, "example.org/blocks");
        pictured(&mut app);
        let nodes = picture_nodes(&app);
        assert_eq!(
            fits(&app),
            vec![(nodes[0], 30, 20), (nodes[1], 20, 20), (nodes[2], 12, 8)]
        );
        assert_eq!(images(&mut app).len(), 3);
        assert_eq!(glyph_text(&mut app), "zz");
        let pixels = window_pixels(&mut app);
        for (at, bgra) in [
            (0, [0, 0, 255, 255]),
            (1, [0, 255, 0, 255]),
            (2, [255, 0, 0, 255]),
        ] {
            assert_eq!(
                pixel(&app, &pixels, picture_point(&app, nodes[at])),
                bgra,
                "picture {at}"
            );
        }
        assert_eq!(notes(&mut app), "1 of the page's 4 pictures is not shown");
        slots_free(&app);
    }

    /// Pictures fitted to the window's width, whose copies at a new width
    /// have no room beside the old ones', are laid again once with the
    /// old let go: every resize shows them all.
    #[test]
    fn copies_refused_beside_the_old_layouts_are_made_with_it_let_go() {
        let table = Table::default()
            .page(
                "https://example.org/wide",
                "text/html",
                "<body style=margin:0><img src=/a.png style=width:100%>\
                 <img src=/b.png style=width:100%>",
            )
            .picture("https://example.org/a.png", &png(100, 100, [1, 2, 3]))
            .picture("https://example.org/b.png", &png(100, 100, [4, 5, 6]));
        let mut app = app(table);
        // Room for both copies at 800 pixels square and their scratch,
        // not for those and the next ones beside.
        app.pictures_term = 7 << 20;
        typed(&mut app, "example.org/wide");
        pictured(&mut app);
        let nodes = picture_nodes(&app);
        let width = |app: &App| fits(app).first().map(|&(_, width, _)| width);
        let first = width(&app).unwrap();
        assert_eq!(fits(&app).len(), 2);
        for (n, wide) in [700, 790, 700, 790].into_iter().enumerate() {
            resize(&mut app, wide);
            pictured(&mut app);
            assert_eq!(fits(&app).len(), 2, "resize {n}");
            assert_eq!(laid(&app).short(), 0, "resize {n}");
            assert_ne!(width(&app), Some(first), "resize {n}");
            // Each copy made at the size it is drawn at.
            for &node in &nodes {
                let copy = crate::page::tests::fit_copy(laid(&app), node);
                let fit = fits(&app).into_iter().find(|f| f.0 == node).unwrap();
                assert_eq!((copy.fitted().width, copy.fitted().height), (fit.1, fit.2));
            }
            assert!(!notes(&mut app).contains("picture"), "resize {n}");
        }
        slots_free(&app);
    }

    /// A `data:` picture longer than td-url's longest URL is known before
    /// it is joined and decoded, with no request.
    #[test]
    fn a_long_data_picture_is_decoded() {
        let mut noise = Vec::new();
        let mut n = 7u32;
        for _ in 0..40 * 40 * 3 {
            n = n.wrapping_mul(1_103_515_245).wrapping_add(12345);
            noise.push((n >> 16) as u8);
        }
        let picture = td_png::encode_rgb8(40, 40, &noise).unwrap();
        let src = base64(&picture);
        assert!(src.len() > 4096, "{}", src.len());
        let markup = format!("<img src=\"  DATA:image/png;base64,{src}\" alt=d>");
        let table = Arc::new(Table::default().page("https://example.org/d", "text/html", &markup));
        let mut app = app_over(table.clone());
        typed(&mut app, "example.org/d");
        pictured(&mut app);
        let nodes = picture_nodes(&app);
        assert_eq!(fits(&app), vec![(nodes[0], 40, 40)]);
        assert_eq!(table.urls(), ["https://example.org/d"]);
    }

    /// A fetched picture holds its slot until the render thread has
    /// decoded it.
    #[test]
    fn a_picture_holds_its_slot_until_it_is_decoded() {
        let a = "https://example.org/a.png";
        let table = Table::default()
            .page("https://example.org/k", "text/html", "<img src=/a.png>")
            .picture(a, &png(3, 3, [1, 1, 1]));
        let (gate, release) = gate(table, &[a]);
        let mut app = app_over(gate.clone());
        typed(&mut app, "example.org/k");
        asked(&mut app, &gate, a);
        let held = hold_render(&app);
        // Fetched; its decoding waits behind the held job.
        release.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        for _ in 0..10 {
            app.poll(0);
        }
        assert_eq!(app.slots.out(), 1);
        assert!(fits(&app).is_empty());
        drop(held);
        pictured(&mut app);
        assert_eq!(fits(&app).len(), 1);
        slots_free(&app);
    }

    /// A picture in the link marked is framed in the selection's colour
    /// over its picture.
    #[test]
    fn a_picture_in_the_marked_link_is_framed() {
        let table = site()
            .page(
                "https://example.org/f",
                "text/html",
                "<body style=margin:0><a href=/b><img src=/a.png></a>",
            )
            .picture("https://example.org/a.png", &png(20, 20, [255, 0, 0]));
        let mut app = app(table);
        typed(&mut app, "example.org/f");
        pictured(&mut app);
        let nodes = picture_nodes(&app);
        let (x, y) = picture_point(&app, nodes[0]);
        let corner = (x - 10, y - 10);
        let pixels = window_pixels(&mut app);
        assert_eq!(pixel(&app, &pixels, corner), [0, 0, 255, 255]);
        key(&mut app, "Tab");
        let pixels = window_pixels(&mut app);
        assert_ne!(pixel(&app, &pixels, corner), [0, 0, 255, 255]);
        assert_eq!(pixel(&app, &pixels, (x, y)), [0, 0, 255, 255]);
    }

    /// Pictures that come while a relayout runs are not settled until it
    /// has been taken: a settle would cancel it for their sake and ask
    /// another.
    #[test]
    fn no_settle_cancels_a_running_relayout() {
        let h = "https://example.org/h.png";
        let table = Table::default()
            .page(
                "https://example.org/n",
                "text/html",
                "<img src=/h.png alt=h><p>text",
            )
            .picture(h, &png(4, 4, [1, 1, 1]));
        let (gate, release) = gate(table, &[h]);
        let mut app = app_over(gate.clone());
        app.clock.fixed = Some(1_000);
        typed(&mut app, "example.org/n");
        asked(&mut app, &gate, h);
        relaid(&mut app);
        let held = hold_render(&app);
        // h is fetched and its decoding waits; a relayout is asked.
        release.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        app.input(Input::Resize(surface(700)), &mut Board::default());
        drop(held);
        // Both done on the render thread, neither taken by the window.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let answered = app
                .current
                .relayout
                .as_ref()
                .unwrap()
                .answer
                .lock()
                .unwrap()
                .is_some();
            if answered && pictures::tests::ready(app.current.pictures.as_ref().unwrap()) > 0 {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "not done");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // One poll takes h while the relayout is out, then the relayout:
        // h waits for the next, none asked for it now.
        app.poll(0);
        assert_eq!(
            pictures::tests::came(app.current.pictures.as_ref().unwrap()),
            1
        );
        assert!(app.current.relayout.is_none() && !app.current.again);
        assert!(fits(&app).is_empty());
        app.clock.fixed = Some(1_000 + pictures::PICTURE_SETTLE);
        pictured(&mut app);
        assert_eq!(fits(&app).len(), 1);
        slots_free(&app);
    }

    /// A relayout that steps down to another tree, or is refused into the
    /// static notice, lets the page's pictures go, their fetches told at
    /// once, before the render thread drops them.
    #[test]
    fn a_step_down_or_refusal_lets_the_pictures_go() {
        for refused in [false, true] {
            let h = "https://example.org/h.png";
            let table = Table::default()
                .page(
                    "https://example.org/s",
                    "text/html",
                    "<p>alpha <img src=/h.png> beta",
                )
                .picture(h, &png(4, 4, [1, 1, 1]));
            let (gate, release) = gate(table, &[h]);
            let mut app = app_over(gate.clone());
            typed(&mut app, "example.org/s");
            asked(&mut app, &gate, h);
            let inbox = pictures::tests::inbox(app.current.pictures.as_ref().unwrap());
            let answer_with = if refused {
                Err(Refusal::Limit("memory"))
            } else {
                let going = Arc::new(AtomicBool::new(false));
                let other = app
                    .renderer
                    .document(
                        Url::parse("https://example.org/s").unwrap(),
                        200,
                        Some("text/html"),
                        b"<p>alpha beta".to_vec(),
                        app.want(),
                        &going,
                    )
                    .unwrap();
                let Body::Page(page) = std::mem::replace(&mut { other }.body, Body::Blank) else {
                    panic!("a page");
                };
                Ok(page)
            };
            let held = hold_render(&app);
            let (_, cancel, answer) = app.renderer.relayout_parts().unwrap();
            *answer.lock().unwrap() = Some(answer_with);
            app.current.relayout = Some(Relayout {
                answer,
                cancel,
                top: app.anchor(),
                focused: None,
                want: app.want(),
                let_go: refused,
            });
            app.poll(0);
            assert!(app.current.pictures.is_none(), "refused {refused}");
            assert!(pictures::tests::gone(&inbox), "refused {refused}");
            assert_eq!(matches!(app.current.doc.body, Body::Resources), refused);
            drop(held);
            release.send(()).unwrap();
            drop(inbox);
            slots_free(&app);
        }
    }
    /// A page shown on rung 3, its text, starts no pictures: it lays
    /// none; on rung 2 it does.
    #[test]
    fn a_text_rung_page_starts_no_pictures() {
        let table = Table::default().page(
            "https://example.org/r3",
            "text/html",
            "<img src=/a.png><p>text",
        );
        let mut app = app(table);
        typed(&mut app, "example.org/r3");
        for (rung, starts) in [(Rung::Text, false), (Rung::UserAgent, true)] {
            app.let_go_pictures();
            app.current.doc.page_mut().unwrap().rung = rung;
            app.start_pictures();
            assert_eq!(app.current.pictures.is_some(), starts, "{rung:?}");
        }
        app.let_go_pictures();
    }
    /// A navigation started while a relayout that let the layout go
    /// runs leaves it running: the page is laid again, never left paper,
    /// while the navigation is out.
    #[test]
    fn a_navigation_leaves_a_let_go_relayout_running() {
        let next = "https://example.org/next";
        let table = Table::default()
            .page(
                "https://example.org/p",
                "text/html",
                &format!("<p>words shown{}", numbered(400)),
            )
            .page(next, "text/html", "<p>next");
        let (gate, release) = gate(table, &[next]);
        let mut app = app_over(gate.clone());
        typed(&mut app, "example.org/p");
        relaid(&mut app);
        let held = hold_render(&app);
        // Too little room beside the layout shown: the relayout lets it
        // go first.
        let root = app.renderer.budget().try_clone().unwrap();
        let hog = root
            .charge(root.limit() - root.used() - (64 << 10))
            .unwrap();
        app.input(Input::Resize(surface(700)), &mut Board::default());
        drop(hog);
        assert!(app.current.laid().is_none());
        let running = app
            .current
            .relayout
            .as_ref()
            .unwrap()
            .cancel
            .try_clone()
            .unwrap();
        type_address(&mut app, "example.org/next");
        asked(&mut app, &gate, next);
        assert!(!running.load(Ordering::SeqCst));
        assert!(!app.current.relay_after);
        drop(held);
        until(&mut app, "the page was not laid again", |app| {
            app.current.laid().is_some()
        });
        assert!(app.loading.is_some());
        assert_eq!(laid(&app).want.width, app.want().width);
        release.send(()).unwrap();
        settle(&mut app);
        assert_eq!(page_text(&app), "next");
    }

    /// A picture whose copy no room ever holds is laid again once a want
    /// after a resize, not again and again.
    #[test]
    fn a_refit_is_asked_once_a_want() {
        let table = Table::default()
            .page(
                "https://example.org/once",
                "text/html",
                "<body style=margin:0><img src=/a.png style=width:25%>\
                 <img src=/b.png style=width:100%>",
            )
            .picture("https://example.org/a.png", &png(100, 100, [1, 2, 3]))
            .picture("https://example.org/b.png", &png(100, 100, [4, 5, 6]));
        let mut app = app(table);
        // Room for a's copy, never for b's.
        app.pictures_term = 2 << 20;
        typed(&mut app, "example.org/once");
        pictured(&mut app);
        let nodes = picture_nodes(&app);
        assert_eq!(fits(&app).len(), 1);
        for wide in [700, 760] {
            resize(&mut app, wide);
            pictured(&mut app);
            assert_eq!(fits(&app).len(), 1, "at {wide}");
            assert_eq!(fits(&app)[0].0, nodes[0]);
            assert_eq!(laid(&app).short(), 1);
            assert_eq!(app.current.refit, Some(laid(&app).want));
            assert!(notes(&mut app).ends_with("1 of the page's 2 pictures is not shown"));
        }
        slots_free(&app);
    }

    /// `data:` pictures go to the render thread one at a time.
    #[test]
    fn data_pictures_take_the_lane_one_at_a_time() {
        let picture = png(3, 3, [0, 255, 0]);
        let src = format!("data:image/png;base64,{}", base64(&picture));
        let markup = format!("<img src=\"{src}\"><img src=\"{src}\"><img src=\"{src}\">");
        let table = Table::default().page("https://example.org/d3", "text/html", &markup);
        let mut app = app(table);
        typed(&mut app, "example.org/d3");
        pictured(&mut app);
        assert_eq!(fits(&app).len(), 3);
        // Asked anew with the render thread held: one on the lane.
        app.let_go_pictures();
        let held = hold_render(&app);
        app.start_pictures();
        for _ in 0..5 {
            app.poll(0);
        }
        assert_eq!(
            pictures::tests::asked(app.current.pictures.as_ref().unwrap()),
            1
        );
        drop(held);
        pictured(&mut app);
        assert_eq!(fits(&app).len(), 3);
    }

    /// A JPEG decoded at a reduced scale is laid at its frame's size.
    #[test]
    fn a_reduced_jpeg_is_laid_at_its_frames_size() {
        let table = Table::default()
            .page(
                "https://example.org/wide",
                "text/html",
                "<body style=margin:0><img src=/w.jpg alt=w>",
            )
            .reply(
                "https://example.org/w.jpg",
                200,
                &[("content-type", "image/jpeg")],
                &crate::picture::tests::wide_jpeg(),
            );
        let mut app = app(table);
        typed(&mut app, "example.org/wide");
        pictured(&mut app);
        let node = picture_nodes(&app)[0];
        let r = laid(&app)
            .lines()
            .iter()
            .flat_map(|line| line.replaced.iter())
            .find(|r| r.node.index() == node)
            .map(|r| (r.width, r.height))
            .unwrap();
        assert_eq!(r, (16384.0, 16.0));
        slots_free(&app);
    }

    /// A refused picture's `alt` text is drawn inside its box, inline and
    /// block-level.
    #[test]
    fn alt_text_is_drawn_within_its_box() {
        let table = Table::default().page(
            "https://example.org/alt",
            "text/html",
            "<body style=margin:0><p>top<p><img src=/gone.png width=120 height=40 alt=qq>\
             <img src=/gone.png style=display:block width=200 height=50 alt=zz>",
        );
        let mut app = app(table);
        typed(&mut app, "example.org/alt");
        pictured(&mut app);
        let nodes = picture_nodes(&app);
        let pane = app.pane();
        let frame = app.current.view.frame(&pane);
        let marks = Marks {
            hits: &[],
            selection: None,
            link: None,
        };
        let mut scratch = Scratch::new(app.renderer.budget());
        let (laid_now, measure) =
            measure_of(&app.current.doc, &mut app.current.measure, &app.renderer).unwrap();
        let mut glyphs = Vec::new();
        paint::paint(
            &laid_now,
            measure,
            &frame,
            app.surface.bounds(),
            &marks,
            &mut scratch,
            &mut |draw| {
                if let Primitive::Glyph { x, y, scalar, .. } = draw.primitive {
                    glyphs.push((x, y, scalar));
                }
            },
        );
        drop(laid_now);
        for (node, letter, (width, height)) in
            [(nodes[0], 'q', (120, 40)), (nodes[1], 'z', (200, 50))]
        {
            let (cx, cy) = picture_point(&app, node);
            let (left, top) = (cx - width / 2, cy - height / 2);
            let drawn: Vec<_> = glyphs.iter().filter(|g| g.2 == letter).collect();
            assert_eq!(drawn.len(), 2, "{letter}");
            for &&(x, y, _) in &drawn {
                assert!(x >= left && x < left + width, "{letter} at {x}, box {left}");
                assert!(y >= top && y < top + height, "{letter} at {y}, box {top}");
            }
        }
    }

    fn base64(bytes: &[u8]) -> String {
        const DIGITS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(DIGITS[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }
}
