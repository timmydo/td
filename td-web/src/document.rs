//! A response as td-web shows it (DESIGN.md §5 and §10): markup parsed,
//! styled and laid out, text laid as a text page, a PNG, JPEG, GIF, WebP
//! or AVIF decoded, anything else named on a page of td-web's own, all of
//! it on the render thread, which also lays a shown page out again and
//! drops what the window lets go.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};

use crate::worker::{Process, SubmitError, Worker};

use crate::fonts::Measure;
use crate::memory::{self, Resident};
use crate::page::Want;
use crate::pipeline::{self, Again, Page, Refusal};
use td_alloc::{Budget, BudgetError, BudgetShared, BudgetString};
use td_ui::picture::Picture;

use crate::charset::{self, Rules};
use crate::picture::{self, Format};
use td_charset::Encoding;
#[cfg(test)]
use td_ui::picture::{MAX_PICTURE_AXIS, MAX_PICTURE_PIXELS};
use td_ui::prose::{Faces, Metrics};
use td_ui::raster::Scale;
use td_url::Url;

/// The most bytes of markup parsed; the rest is not shown.
pub const MAX_MARKUP: usize = 2 << 20;
/// The most bytes of a text page, its prefix among them.
pub const MAX_TEXT: usize = pipeline::MAX_TEXT;
/// The resource header sniffing reads (MIME Sniffing).
const SNIFF_HEADER: usize = 1445;

/// `text` with its controls and line separators percent-encoded, as the
/// status row and the address show a URL's text.
pub fn visible(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut bytes = [0u8; 4];
    for c in text.chars() {
        if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
            for byte in c.encode_utf8(&mut bytes).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// What a document is shown as.
pub enum Body {
    /// The start page: nothing yet.
    Blank,
    Page(Page),
    Picture(Resident<Picture>),
    /// Rung 4: a static notice, for want of memory or a page refused at
    /// every rung.
    Resources,
}

pub struct Document {
    /// The URL that answered; none for an address td-web did not take.
    pub url: Option<Url>,
    pub status: u16,
    pub title: Option<String>,
    /// What the status row says once it has nothing better to say: the
    /// encoding, how much of the reply was read.
    pub note: Option<String>,
    /// The reply's `content-disposition`, which names a saved file.
    pub disposition: Option<String>,
    pub body: Body,
    /// A reply td-web does not show, kept as it arrived for a save
    /// (DESIGN.md §5, "Saving"): at most `MAX_DOCUMENT` beside a page of
    /// a few lines.
    held: Option<Resident<Vec<u8>>>,
    /// Told the thread the document is dropped on, in tests.
    #[cfg(test)]
    pub recorder: Option<std::sync::mpsc::Sender<std::thread::ThreadId>>,
}

#[cfg(test)]
impl Drop for Document {
    fn drop(&mut self) {
        if let Some(recorder) = &self.recorder {
            let _ = recorder.send(std::thread::current().id());
        }
    }
}

/// What a reply is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Html,
    Text,
    Picture(Format),
    Other,
}

/// A page's own measure on the render thread: the window's faces at the
/// page's scale and face, its caches charged to the page.
fn measure_for(faces: Option<&Faces>, want: Want, memory: &Budget) -> Measure {
    let scale = Scale::new(want.scale).unwrap_or_default();
    Measure::new(Metrics::new_in(faces, scale, memory), want.body)
}

impl Document {
    fn with_body(url: Option<Url>, status: u16, body: Body) -> Self {
        Self {
            url,
            status,
            title: None,
            note: None,
            disposition: None,
            body,
            held: None,
            #[cfg(test)]
            recorder: None,
        }
    }

    fn resources(url: Option<Url>, status: u16) -> Self {
        Self::with_body(url, status, Body::Resources)
    }

    pub fn blank() -> Self {
        Self::with_body(None, 0, Body::Blank)
    }

    /// A page td-web writes from `why`, laid for `want` on this thread,
    /// or rung 4's notice.
    pub fn own(
        url: Option<Url>,
        status: u16,
        why: &str,
        want: Want,
        faces: Option<&Faces>,
        root: &Budget,
    ) -> Self {
        let made = (|| {
            let memory = root.child(root.limit()).map_err(Refusal::from)?;
            let mut measure = measure_for(faces, want, &memory);
            let never = AtomicBool::new(false);
            pipeline::own_page(why, want, &mut measure, &never, &memory)
        })();
        match made {
            Ok(page) => Self::with_body(url, status, Body::Page(page)),
            Err(_) => Self::resources(url, status),
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn from_reply_in(
        url: Url,
        status: u16,
        content_type: Option<&str>,
        body: Vec<u8>,
        want: Want,
        faces: Option<&Faces>,
        stopped: &AtomicBool,
        root: &Budget,
    ) -> Option<Self> {
        Self::from_reply(
            url,
            status,
            content_type,
            body,
            want,
            faces,
            stopped,
            root,
            None,
        )
    }

    /// `body`, answered with `status` from `url` as `content_type`, made
    /// into a document for `want`; none once `stopped` is seen. With
    /// `room`, a picture the root has no room for hands its body back
    /// there, and none, so the window can let the page shown go and the
    /// picture be tried once more (DESIGN.md §10, "Memory").
    #[allow(clippy::too_many_arguments)]
    fn from_reply(
        url: Url,
        status: u16,
        content_type: Option<&str>,
        body: Vec<u8>,
        want: Want,
        faces: Option<&Faces>,
        stopped: &AtomicBool,
        root: &Budget,
        room: Option<&mut Option<(Url, Vec<u8>)>>,
    ) -> Option<Self> {
        if body.is_empty() && !(200..300).contains(&status) {
            let why = format!("The server answered {status} and sent nothing to show.");
            return Some(Self::own(Some(url), status, &why, want, faces, root));
        }
        let media = content_type
            .and_then(|value| value.split(';').next())
            .map(|media| media.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let kind = match media.as_str() {
            "text/html" | "application/xhtml+xml" => Kind::Html,
            "" | "application/octet-stream" => sniff(&body),
            _ if media.starts_with("text/") => Kind::Text,
            _ => Format::from_mime(&media)
                .map(Kind::Picture)
                .unwrap_or(Kind::Other),
        };
        if stopped.load(Ordering::SeqCst) {
            return None;
        }
        let Ok(memory) = root.child(root.limit()) else {
            return Some(Self::resources(Some(url), status));
        };
        let size = body.len();
        match kind {
            Kind::Html | Kind::Text => {
                // XML, as markup or shown as text, takes XML's rules.
                let xml = media == "text/xml" || media.ends_with("+xml");
                let (rules, most) = match kind {
                    Kind::Html if xml => (Rules::Xml, MAX_MARKUP),
                    Kind::Html => (Rules::Html, MAX_MARKUP),
                    // A text page's markup is its prefix and its text.
                    _ if xml => (Rules::Xml, MAX_TEXT - crate::tree::TEXT_PREFIX.len()),
                    _ => (Rules::Text, MAX_TEXT - crate::tree::TEXT_PREFIX.len()),
                };
                let declared = content_type.and_then(charset::label);
                let declared = declared.as_ref().map(charset::Label::as_bytes);
                let Ok(decoded) = charset::decode(body, declared, rules, most) else {
                    return Some(Self::resources(Some(url), status));
                };
                let note = noted(decoded.encoding, decoded.read, size);
                let mut measure = measure_for(faces, want, &memory);
                let made = if kind == Kind::Text {
                    pipeline::text_page(&decoded.text, want, &mut measure, stopped, &memory)
                } else {
                    let markup = BudgetString::copy(&memory, &decoded.text)
                        .and_then(|markup| BudgetShared::new(&memory, markup));
                    drop(decoded);
                    match markup {
                        Ok(markup) => {
                            pipeline::markup_page(markup, want, &mut measure, stopped, &memory)
                        }
                        Err(_) => Err(Refusal::Limit("memory")),
                    }
                };
                drop(measure);
                match made {
                    Ok(page) => {
                        let title = page.tree.title();
                        let title = (!title.is_empty()).then(|| title.to_string());
                        let mut doc = Self::with_body(Some(url), status, Body::Page(page));
                        doc.title = title;
                        doc.note = note;
                        Some(doc)
                    }
                    Err(Refusal::Cancelled) => None,
                    Err(Refusal::Limit(_)) => Some(Self::resources(Some(url), status)),
                }
            }
            Kind::Picture(format) => match picture::decode_with_budget(format, &body, &memory) {
                Ok(picture) => Some(Self::with_body(Some(url), status, Body::Picture(picture))),
                Err(picture::Error::Room) if room.is_some() => {
                    if let Some(room) = room {
                        *room = Some((url, body));
                    }
                    None
                }
                Err(why) if why.resources() => Some(Self::resources(Some(url), status)),
                Err(why) => {
                    let why = format!("{why}");
                    Some(Self::own(Some(url), status, &why, want, faces, root))
                }
            },
            Kind::Other => {
                let named = if media.is_empty() {
                    "of no stated type".to_string()
                } else {
                    visible(media.get(..media.floor_char_boundary(4096)).unwrap_or(""))
                };
                // Only what the server answered with success is offered
                // for saving, and kept for it.
                let held = ((200..300).contains(&status) && !body.is_empty()).then_some(body);
                let offer = if held.is_some() {
                    "Ctrl+S saves it in Downloads.\n\n"
                } else {
                    ""
                };
                let said = format!(
                    "This is {named}, {size} bytes.\n\n{offer}td-web shows HTML, text, PNG, \
                     JPEG, GIF, WebP and AVIF; Firefox opens the rest."
                );
                let held = match held {
                    Some(body) => {
                        let bytes = body.capacity();
                        let owned = memory
                            .charge(bytes)
                            .and_then(|charge| Resident::new(body, charge, bytes));
                        match owned {
                            Ok(body) => Some(body),
                            Err(_) => return Some(Self::resources(Some(url), status)),
                        }
                    }
                    None => None,
                };
                let mut doc = Self::own(Some(url), status, &said, want, faces, &memory);
                doc.held = held;
                Some(doc)
            }
        }
    }

    /// A picture's fitted copy at a size, charged to its budget; none
    /// when refused.
    pub fn fit_picture(
        &self,
        width: usize,
        height: usize,
    ) -> Option<Resident<td_ui::picture::Fitted>> {
        let Body::Picture(picture) = &self.body else {
            return None;
        };
        let charge = picture
            .budget()
            .charge(td_ui::picture::MAX_PICTURE_PIXELS * 4)
            .ok()?;
        let fitted = picture.fit(width, height)?;
        let bytes = fitted.bgra.capacity();
        Resident::new(fitted, charge, bytes).ok()
    }

    /// The body of a reply td-web does not show, given up to be saved;
    /// a later save fetches it again.
    pub fn take_held(&mut self) -> Option<Resident<Vec<u8>>> {
        self.held.take()
    }

    pub fn page(&self) -> Option<&Page> {
        match &self.body {
            Body::Page(page) => Some(page),
            _ => None,
        }
    }

    pub fn page_mut(&mut self) -> Option<&mut Page> {
        match &mut self.body {
            Body::Page(page) => Some(page),
            _ => None,
        }
    }
}

/// Common media types fit inside the charged request slot. Longer headers
/// retain exact UTF-8 bytes in a separately charged owner.
enum ContentType {
    Inline(td_alloc::SmallText<128>),
    Long(td_alloc::BudgetString),
}
impl ContentType {
    fn copy(memory: &Budget, text: &str) -> Result<Self, BudgetError> {
        match td_alloc::SmallText::copy(text) {
            Some(text) => Ok(Self::Inline(text)),
            None => td_alloc::BudgetString::copy(memory, text).map(Self::Long),
        }
    }
    fn as_str(&self) -> &str {
        match self {
            Self::Inline(text) => text.as_str(),
            Self::Long(text) => text,
        }
    }
}

/// A typed render request; its input body and URL retain their existing
/// fetch/metadata limits. The content type is a budget-owned handoff copy.
struct Reply {
    url: Url,
    status: u16,
    kind: Option<ContentType>,
    body: Vec<u8>,
    want: Want,
    faces: SharedFaces,
    stopped: Arc<AtomicBool>,
    /// A picture refused for room hands its reply back, once.
    ask: bool,
}

/// A navigated picture the root had no room for, handed back so the
/// window can let the page shown go first and try it once more.
pub struct Retry {
    reply: Reply,
    /// The URL an error page names, as `navigated` kept it.
    shown: Url,
}

impl Retry {
    /// The picture is not tried again: §5's static notice.
    pub fn refused(self) -> Document {
        Document::resources(Some(self.reply.url), self.reply.status)
    }
}

type Rendered = Option<Result<Document, Reply>>;

fn render_reply(reply: Reply, memory: &Budget) -> Rendered {
    let Reply {
        url,
        status,
        kind,
        body,
        want,
        faces,
        stopped,
        ask,
    } = reply;
    let mut room = None;
    let made = Document::from_reply(
        url,
        status,
        kind.as_ref().map(ContentType::as_str),
        body,
        want,
        faces.as_ref().as_ref(),
        &stopped,
        memory,
        ask.then_some(&mut room),
    );
    if stopped.load(Ordering::SeqCst) {
        return None;
    }
    match (made, room) {
        (Some(document), _) => Some(Ok(document)),
        (None, Some((url, body))) => Some(Err(Reply {
            url,
            status,
            kind,
            body,
            want,
            faces,
            stopped,
            ask: false,
        })),
        (None, None) => None,
    }
}

/// A relayout's answer, which the window takes on a later poll.
pub type Answer = BudgetShared<Mutex<Option<Result<Page, Refusal>>>>;
/// A relayout's cancel flag, the window's and the render thread's.
pub type Cancel = BudgetShared<AtomicBool>;
/// The faces both threads measure with, shared.
pub type SharedFaces = BudgetShared<Option<Faces>>;

/// What the window hands the render thread without waiting; the lane
/// holds `LANE` of them by value.
#[allow(clippy::large_enum_variant)]
pub enum Lane {
    /// A page or layout let go, dropped here so its last drop is the
    /// render thread's.
    Drop(Dropped),
    /// A page laid out again, answered through `answer`.
    Again {
        again: Again,
        faces: SharedFaces,
        cancel: Cancel,
        answer: Answer,
    },
}

/// Whatever the window lets go, held by value as `Lane` is.
#[allow(clippy::large_enum_variant)]
pub enum Dropped {
    Document(Document),
    Page(Page),
    /// A navigation's answer, with whatever it holds or is yet sent it.
    Arrival(Receiver<Option<(Document, Option<String>)>>),
}

/// Answers a relayout that ends without answering, as one whose job
/// panicked does, so the window is not left waiting for it.
struct Unanswered<'a>(&'a Answer);

impl Drop for Unanswered<'_> {
    fn drop(&mut self) {
        let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(Err(Refusal::Limit("the render thread failed")));
        }
    }
}

fn lane(item: Lane, _: &Budget) {
    match item {
        Lane::Drop(Dropped::Document(document)) => drop(document),
        Lane::Drop(Dropped::Page(page)) => drop(page),
        Lane::Drop(Dropped::Arrival(answer)) => drop(answer),
        Lane::Again {
            again,
            faces,
            cancel,
            answer,
        } => {
            let _unanswered = Unanswered(&answer);
            #[cfg(test)]
            let width = again.want.width;
            #[cfg(test)]
            tests::hook(width);
            let made = if cancel.load(Ordering::SeqCst) {
                drop(again);
                Err(Refusal::Cancelled)
            } else {
                let mut measure = measure_for(faces.as_ref().as_ref(), again.want, &again.budget);
                pipeline::again(again, &mut measure, &cancel)
            };
            #[cfg(test)]
            tests::made(width, &cancel);
            // Under the slot's lock, as the window cancels and takes: a
            // page made for a relayout the window has let go is dropped
            // here, never left in a slot whose last handle is the window's.
            let mut slot = answer.lock().unwrap_or_else(PoisonError::into_inner);
            *slot = Some(if cancel.load(Ordering::SeqCst) {
                Err(Refusal::Cancelled)
            } else {
                made
            });
        }
    }
}

type Thread = Worker<Reply, Rendered, Lane>;

/// One worker serializes rendering, with a fixed queue for fetch workers
/// and a lane the window posts to without waiting. The reusable slots are
/// admitted on its page root at startup.
pub struct Renderer(BudgetShared<RenderState>);
struct RenderState {
    thread: Mutex<Thread>,
    memory: Budget,
    faces: SharedFaces,
    /// What `let_go` could not post yet, `release` posts.
    pending: Mutex<[Option<Dropped>; PENDING]>,
}

// `run` holds the thread's lock across `submit`, which waits for a free
// slot: with no more fetch threads than slots, one is always free, so the
// window's `post` never waits behind it.
const _: () = assert!(crate::app::MAX_WORKERS <= crate::worker::QUEUED);

/// How many let-goes are held while the lane is full.
const PENDING: usize = 8;
#[cfg(test)]
impl Clone for Renderer {
    #[allow(clippy::unwrap_used)]
    fn clone(&self) -> Self {
        self.try_clone().unwrap()
    }
}

fn render_thread(memory: &Budget) -> Result<Thread, ()> {
    Worker::start(
        memory,
        Process {
            request: render_reply,
            lane,
        },
    )
}

impl Renderer {
    /// A renderer and its thread measuring with `faces`, or why the
    /// thread could not start.
    pub fn start(faces: Option<Faces>) -> Result<Self, String> {
        let memory = memory::root()
            .map_err(|_| td_alloc::string("no memory for page budgets").unwrap_or_default())?;
        Self::with_budget(&memory, faces)
    }
    pub fn with_budget(memory: &Budget, faces: Option<Faces>) -> Result<Self, String> {
        let copied = memory
            .try_clone()
            .map_err(|_| td_alloc::string("no page budget handle").unwrap_or_default())?;
        let thread = render_thread(memory).map_err(|_| {
            td_alloc::string("the render thread could not start").unwrap_or_default()
        })?;
        let faces = BudgetShared::new(memory, faces)
            .map_err(|_| td_alloc::string("no memory for the faces' handle").unwrap_or_default())?;
        let state = RenderState {
            thread: Mutex::new(thread),
            memory: copied,
            faces,
            pending: Mutex::new(std::array::from_fn(|_| None)),
        };
        BudgetShared::new(memory, state)
            .map(Self)
            .map_err(|_| td_alloc::string("no memory for renderer ownership").unwrap_or_default())
    }
    pub fn try_clone(&self) -> Result<Self, BudgetError> {
        Ok(Self(self.0.try_clone()?))
    }
    pub fn budget(&self) -> &Budget {
        &self.0.memory
    }
    pub fn faces(&self) -> Option<&Faces> {
        self.0.faces.as_ref().as_ref()
    }
    /// A relayout's parts: the faces, a cancel flag and an answer slot,
    /// in the renderer's budget.
    pub fn relayout_parts(&self) -> Result<(SharedFaces, Cancel, Answer), BudgetError> {
        Ok((
            self.0.faces.try_clone()?,
            BudgetShared::new(self.budget(), AtomicBool::new(false))?,
            BudgetShared::new(self.budget(), Mutex::new(None))?,
        ))
    }
    /// A page td-web writes, laid on the calling thread.
    pub fn error(&self, url: Option<Url>, why: &str, want: Want) -> Document {
        Document::own(url, 0, why, want, self.faces(), self.budget())
    }

    /// Claim a reusable slot, restarting a lost worker before retrying.
    /// Waiting for its answer does not hold the submission lock.
    fn run(&self, reply: Reply) -> Result<Option<Rendered>, BudgetError> {
        let ticket = {
            let mut thread = self.0.thread.lock().unwrap_or_else(PoisonError::into_inner);
            match thread.submit(reply) {
                Ok(ticket) => ticket,
                Err(SubmitError::Memory(error, _)) => return Err(error),
                Err(SubmitError::Closed(reply)) => {
                    let Ok(replacement) = render_thread(self.budget()) else {
                        return Ok(None);
                    };
                    *thread = replacement;
                    match thread.submit(reply) {
                        Ok(ticket) => ticket,
                        Err(SubmitError::Memory(error, _)) => return Err(error),
                        Err(SubmitError::Closed(_)) => return Ok(None),
                    }
                }
            }
        };
        Ok(ticket.wait())
    }

    /// Hands `item` to the render thread's lane without waiting; a full
    /// lane hands it back. A lost render thread is started again first,
    /// as `run` does.
    #[allow(clippy::result_large_err)]
    pub fn post(&self, item: Lane) -> Result<(), Lane> {
        let mut thread = self.0.thread.lock().unwrap_or_else(PoisonError::into_inner);
        match thread.post(item) {
            Err(item) if thread.closed() => match render_thread(self.budget()) {
                Ok(replacement) => {
                    *thread = replacement;
                    thread.post(item)
                }
                Err(()) => Err(item),
            },
            posted => posted,
        }
    }

    /// Lets `dropped` go on the render thread: posted, else held until
    /// `release` posts it. Only with the lane and `PENDING` held items
    /// all full is it dropped here, which takes a burst of let-goes
    /// while one relayout runs.
    pub fn let_go(&self, dropped: Dropped) {
        self.release();
        let Err(Lane::Drop(dropped)) = self.post(Lane::Drop(dropped)) else {
            return;
        };
        let mut pending = self
            .0
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(free) = pending.iter_mut().find(|held| held.is_none()) {
            *free = Some(dropped);
        } else {
            drop(pending);
            drop(dropped);
        }
    }

    /// Whether the render thread was lost and could not be started
    /// again, so nothing `let_go` holds is posted.
    pub fn lost(&self) -> bool {
        self.0
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed()
    }

    /// Whether `let_go` holds anything not yet posted.
    pub fn holding(&self) -> bool {
        self.0
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(Option::is_some)
    }

    /// Posts what `let_go` held for want of room in the lane, in order;
    /// the window calls it at every poll.
    pub fn release(&self) {
        for at in 0..PENDING {
            let held = {
                let mut pending = self
                    .0
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                pending.get_mut(at).and_then(Option::take)
            };
            let Some(held) = held else {
                continue;
            };
            if let Err(Lane::Drop(held)) = self.post(Lane::Drop(held)) {
                let mut pending = self
                    .0
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some(slot) = pending.get_mut(at) {
                    *slot = Some(held);
                }
                return;
            }
        }
    }

    /// The reply made into a document for `want` on the render thread,
    /// waiting for it there behind the replies handed over before it; an
    /// error page when it could not be made there. A document whose
    /// navigation was stopped while it was made is dropped there too. A
    /// picture the root has no room for is handed back to be tried again,
    /// by value as the worker's answers are.
    #[allow(clippy::too_many_arguments, clippy::result_large_err)]
    pub fn navigated(
        &self,
        url: Url,
        status: u16,
        content_type: Option<&str>,
        body: Vec<u8>,
        want: Want,
        stopped: &Arc<AtomicBool>,
    ) -> Option<Result<Document, Retry>> {
        if stopped.load(Ordering::SeqCst) {
            return None;
        }
        let shown = url.clone();
        let made = (|| {
            let kind = content_type
                .map(|kind| ContentType::copy(self.budget(), kind))
                .transpose()?;
            let reply = Reply {
                url,
                status,
                kind,
                body,
                want,
                faces: self.0.faces.try_clone()?,
                stopped: Arc::clone(stopped),
                ask: true,
            };
            self.run(reply)
        })();
        match made {
            Ok(Some(Some(Err(reply)))) => Some(Err(Retry { reply, shown })),
            made => self.answered(made, shown, status, want).map(Ok),
        }
    }

    /// A picture handed back made again, the page shown let go; this
    /// time one refused for room is §5's static notice.
    pub fn retry(&self, retry: Retry) -> Option<Document> {
        let Retry { reply, shown } = retry;
        if reply.stopped.load(Ordering::SeqCst) {
            return None;
        }
        let (status, want) = (reply.status, reply.want);
        let made = self.run(Reply {
            ask: false,
            ..reply
        });
        self.answered(made, shown, status, want)
    }

    fn answered(
        &self,
        made: Result<Option<Rendered>, BudgetError>,
        shown: Url,
        status: u16,
        want: Want,
    ) -> Option<Document> {
        match made {
            Ok(Some(Some(Ok(document)))) => Some(document),
            // Refused for room again: §5's static notice.
            Ok(Some(Some(Err(reply)))) => Some(Document::resources(Some(reply.url), status)),
            Ok(Some(None)) => None,
            Ok(None) => Some(self.error(Some(shown), "the page could not be rendered", want)),
            Err(_) => Some(Document::resources(Some(shown), status)),
        }
    }

    /// `navigated`, a picture refused for room shown refused.
    #[cfg(test)]
    pub fn document(
        &self,
        url: Url,
        status: u16,
        content_type: Option<&str>,
        body: Vec<u8>,
        want: Want,
        stopped: &Arc<AtomicBool>,
    ) -> Option<Document> {
        self.navigated(url, status, content_type, body, want, stopped)
            .map(|made| made.unwrap_or_else(Retry::refused))
    }

    /// The render thread stopped, as a lost one is.
    #[cfg(test)]
    pub fn stop_thread(&self) {
        self.0
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stop();
    }

    #[cfg(test)]
    pub fn idle(&self) {
        self.0
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .idle();
    }
}

// The worker catches a rendering's panic, which only an unwinding
// build can: a build that aborts on panic (`panic = "abort"` in a profile,
// `-C panic=abort` from a recipe) would end td-web on any rendering's
// panic, so it does not build.
#[cfg(not(panic = "unwind"))]
compile_error!("td-web's render thread catches a rendering's panic: its build must unwind");

/// What the status row notes of a page's text (DESIGN.md §8): the
/// encoding it was decoded from when that is not UTF-8, and how much of
/// it is shown when not all of it. Only a note, so none when there is no
/// memory for it.
fn noted(encoding: Encoding, read: usize, size: usize) -> Option<String> {
    let named = (encoding != Encoding::Utf8).then(|| encoding.name());
    if read >= size {
        return named.and_then(|name| td_alloc::string(name).ok());
    }
    let (name, between) = named.map_or(("", ""), |name| (name, " · "));
    td_alloc::format(format_args!(
        "{name}{between}only the first {read} of the page's {size} bytes are shown"
    ))
    .ok()
}

/// What a body of no stated type is: HTML by its start, a picture by its
/// signature, and text by a byte order mark or no byte of binary data in
/// its first `SNIFF_HEADER` bytes, the resource header (MIME Sniffing,
/// "Rules for distinguishing if a resource is text or binary").
fn sniff(body: &[u8]) -> Kind {
    let header = body.get(..SNIFF_HEADER).unwrap_or(body);
    let unmarked = body.strip_prefix(b"\xef\xbb\xbf").unwrap_or(body);
    let mut start = [0u8; 15];
    let mut len = 0;
    for &byte in unmarked
        .iter()
        .skip_while(|b| b.is_ascii_whitespace())
        .take(15)
    {
        if let Some(slot) = start.get_mut(len) {
            *slot = byte.to_ascii_lowercase();
            len += 1;
        }
    }
    let start = start.get(..len).unwrap_or_default();
    if start.starts_with(b"<!doctype html") || start.starts_with(b"<html") {
        Kind::Html
    } else if let Some(format) = Format::sniff(body) {
        Kind::Picture(format)
    } else if [b"\xfe\xff".as_slice(), b"\xff\xfe", b"\xef\xbb\xbf"]
        .iter()
        .any(|mark| body.starts_with(mark))
        || !header
            .iter()
            .any(|byte| matches!(byte, 0..=8 | 0x0b | 0x0e..=0x1a | 0x1c..=0x1f))
    {
        Kind::Text
    } else {
        Kind::Other
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
pub(crate) mod tests {
    use super::*;

    /// A relayout for this width panics on the render thread.
    pub(crate) const PANICS: f64 = 12_345.0;
    /// A relayout for this width waits on the render thread while `HOLD`.
    pub(crate) const WAITS: f64 = 23_456.0;
    pub(crate) static HOLD: AtomicBool = AtomicBool::new(false);
    /// Held by a test that raises `HOLD`, so no other test's lowers it.
    static HOLDING: Mutex<()> = Mutex::new(());

    /// `HOLD` raised until the guard goes, a failed assertion's unwinding
    /// included, so a held render thread is let go before it is joined.
    pub(crate) fn hold() -> impl Drop {
        struct Held(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
        impl Drop for Held {
            fn drop(&mut self) {
                HOLD.store(false, Ordering::SeqCst);
            }
        }
        let one = HOLDING.lock().unwrap_or_else(PoisonError::into_inner);
        HOLD.store(true, Ordering::SeqCst);
        Held(one)
    }

    /// What the lane does for a relayout of one of the widths above.
    pub(crate) fn hook(width: f64) {
        if width == PANICS {
            panic!("a relayout's job panics");
        }
        if width == WAITS {
            while HOLD.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    /// A relayout for this width is cancelled once made, before the lane
    /// answers it.
    pub(crate) const CANCELLED: f64 = 34_567.0;

    /// What the lane does once a relayout of one of the widths above is
    /// made.
    pub(crate) fn made(width: f64, cancel: &AtomicBool) {
        if width == CANCELLED {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    /// A relayout's guard, as the lane holds it.
    pub(crate) fn unanswered(answer: &Answer) -> impl Drop + '_ {
        Unanswered(answer)
    }
    use crate::page::tests::WANT;
    use crate::page::Laid;
    use crate::pipeline::Rung;
    use crate::text;
    use crate::tree::Source;
    use std::ops::ControlFlow;
    use td_css::values::{ComputedValue, Property};

    fn base() -> Url {
        Url::parse("https://example.org/docs/page").unwrap()
    }

    /// `body` answered with `status` as `content_type`, made into a
    /// document on this thread under a root of its own.
    fn reply(status: u16, content_type: Option<&str>, body: &[u8]) -> Document {
        let root = memory::root().unwrap();
        Document::from_reply_in(
            base(),
            status,
            content_type,
            body.to_vec(),
            WANT,
            None,
            &AtomicBool::new(false),
            &root,
        )
        .unwrap()
    }

    fn html_doc(body: &[u8]) -> Document {
        reply(200, Some("text/html"), body)
    }

    /// Everything a laid page holds, as a copy of it all takes it.
    fn copy_of(laid: &Laid) -> String {
        let (start, end) = text::page_bounds(laid);
        let mut out = String::new();
        let _ = text::copy(laid, start, end, &mut |piece| {
            out.push_str(piece);
            ControlFlow::Continue(())
        });
        out
    }

    /// A document's page's text; it must be a page.
    fn text_of(doc: &Document) -> String {
        let page = doc.page().expect("a page");
        copy_of(page.laid.as_ref().expect("laid"))
    }

    /// `body` answered as `content_type`: its text and the note the
    /// status row shows.
    fn shown(content_type: &str, body: &[u8]) -> (String, Option<String>) {
        let doc = reply(200, Some(content_type), body);
        (text_of(&doc), doc.note.clone())
    }

    fn picture(doc: &Document) -> Option<&Picture> {
        match &doc.body {
            Body::Picture(picture) => Some(picture),
            _ => None,
        }
    }

    fn resources(doc: &Document) -> bool {
        matches!(doc.body, Body::Resources)
    }

    #[test]
    fn controls_but_the_tab_are_dropped_and_crlf_ends_a_line() {
        let doc = reply(
            200,
            Some("text/plain"),
            "a\x01b\rc\x1bd\x7fe\u{85}f\r\nnext\tx".as_bytes(),
        );
        assert_eq!(text_of(&doc), "ab\ncdef\nnext\tx");
        // Markup keeps its controls, which td-layout shows as U+FFFD
        // (DESIGN.md §10, "Places, find, selection and links").
        let doc = html_doc(b"<pre>one\r\ntwo\x0cthree</pre>");
        assert_eq!(text_of(&doc), "one\ntwo\u{fffd}three");
    }

    /// A text page is one text node after td-web's prefix, laid
    /// `pre-wrap`, and a copy of it all is its text with CR LF and a lone
    /// CR made LF and every control but the tab dropped.
    #[test]
    fn a_text_page_is_one_text_node_laid_pre_wrap() {
        let text = "first  line\r\nsecond\rthird\t\x07tabbed\u{9b}\n";
        let doc = reply(200, Some("text/plain"), text.as_bytes());
        let page = doc.page().unwrap();
        assert_eq!(page.tree.source(), Source::Text);
        assert_eq!(page.rung, Rung::Own);
        assert!(page.markup.is_none());
        let document = page.tree.document();
        let mut texts = Vec::new();
        let mut at = Some(document.root());
        while let Some(id) = at {
            if crate::tree::is_text(document, id) {
                texts.push(document.node(id).unwrap().text().unwrap());
            }
            at = page.tree.next(id, false);
        }
        let expected = "first  line\nsecond\nthird\ttabbed\n";
        assert_eq!(texts, [expected]);
        let laid = page.laid.as_ref().unwrap();
        let fragment = laid
            .lines()
            .iter()
            .find_map(|line| line.fragments.iter().next())
            .unwrap();
        let style = laid.styled().style(fragment.style).unwrap();
        assert!(matches!(
            style.get(Property::WhiteSpace),
            Some(ComputedValue::Keyword("pre-wrap"))
        ));
        assert_eq!(copy_of(laid), expected.trim_end_matches('\n'));
        assert!(doc.title.is_none() && doc.note.is_none());
    }

    #[test]
    fn a_page_of_no_stated_type_is_sniffed() {
        let doc = reply(200, None, b"\n <!DOCTYPE html><p>x");
        assert_eq!(doc.page().unwrap().tree.source(), Source::Markup);
        assert_eq!(text_of(&doc), "x");
        let doc = reply(200, None, b"\xef\xbb\xbf<html><p>y");
        assert_eq!(text_of(&doc), "y", "past a byte order mark");
        let doc = reply(200, None, b"plain\ttab");
        assert_eq!(doc.page().unwrap().tree.source(), Source::Text);
        assert_eq!(text_of(&doc), "plain\ttab");
        let doc = reply(200, None, &[0, 1, 2]);
        assert!(text_of(&doc).contains("of no stated type, 3 bytes"));
        // Text in another encoding has no byte of binary data; text in
        // UTF-16 says so by its byte order mark.
        let doc = reply(200, None, b"caf\xe9\x1b");
        assert_eq!(text_of(&doc), "café");
        let doc = reply(200, None, b"\xff\xfeo\x00k\x00");
        assert_eq!(text_of(&doc), "ok");
        let doc = reply(200, None, b"a\x0bb");
        assert!(text_of(&doc).contains("of no stated type"));
        // Only the resource header is read for binary data.
        let mut late = vec![b'a'; SNIFF_HEADER];
        late.push(0);
        let doc = reply(200, None, &late);
        assert_eq!(text_of(&doc), "a".repeat(SNIFF_HEADER));
        late[SNIFF_HEADER - 1] = 0;
        let doc = reply(200, None, &late);
        assert!(text_of(&doc).contains("of no stated type"));
    }

    /// XML shown as text takes XML's rules: UTF-8, not windows-1252,
    /// when it has no label.
    #[test]
    fn xml_shown_as_text_defaults_to_utf8() {
        for media in ["text/xml", "text/vnd.example+xml"] {
            assert_eq!(
                shown(media, b"<a>caf\xe9</a>"),
                ("<a>caf\u{fffd}</a>".into(), None),
                "{media}"
            );
        }
        assert_eq!(
            shown("text/xml; charset=iso-8859-1", b"<a>caf\xe9</a>"),
            ("<a>café</a>".into(), Some("windows-1252".into()))
        );
    }

    /// A page in another encoding is decoded by the Content-Type's
    /// charset, else a `<meta>` naming one, its `<title>` too, and the
    /// status row names the encoding.
    #[test]
    fn a_legacy_page_is_decoded_by_its_label_else_its_meta() {
        let page = b"<title>Caf\xe9</title><p>\x80 5, caf\xe9";
        let doc = reply(200, Some("text/html; charset=windows-1252"), page);
        assert_eq!(doc.title.as_deref(), Some("Café"));
        assert_eq!(text_of(&doc), "€ 5, café");
        assert_eq!(doc.note.as_deref(), Some("windows-1252"));
        assert_eq!(
            shown("text/html;charset=\"ISO-8859-1\"", page).0,
            "€ 5, café",
            "Latin-1 is windows-1252"
        );
        let (text, note) = shown("text/html", b"<meta charset=windows-1252><p>caf\xe9 \x80");
        assert_eq!(
            (text.as_str(), note.as_deref()),
            ("café €", Some("windows-1252"))
        );
        let page = b"<head><meta http-equiv=\"Content-Type\" \
                     content=\"text/html; charset=iso-8859-15\">\
                     <title>\xa4uro</title></head><p>\xa4";
        let doc = html_doc(page);
        assert_eq!(doc.title.as_deref(), Some("€uro"));
        assert_eq!(text_of(&doc), "€");
        assert_eq!(doc.note.as_deref(), Some("ISO-8859-15"));
    }

    #[test]
    fn shift_jis_and_gbk_pages_are_decoded() {
        let (text, note) = shown(
            "text/html; charset=Shift_JIS",
            b"<p>\x93\xfa\x96{\x8c\xea \xb1",
        );
        assert_eq!(text, "日本語 ｱ");
        assert_eq!(note.as_deref(), Some("Shift_JIS"));
        let (text, note) = shown("text/html", b"<meta charset=gb2312><p>\xd6\xd0\xce\xc4");
        assert_eq!(text, "中文");
        assert_eq!(note.as_deref(), Some("GBK"));
    }

    /// A byte order mark decides, over the label, and is not shown.
    #[test]
    fn a_byte_order_mark_wins_over_the_label() {
        let mut page = b"\xff\xfe".to_vec();
        for unit in "<p>é€</p>".encode_utf16() {
            page.extend(unit.to_le_bytes());
        }
        let (text, note) = shown("text/html", &page);
        assert_eq!((text.as_str(), note.as_deref()), ("é€", Some("UTF-16LE")));
        let (text, note) = shown(
            "text/html; charset=windows-1252",
            b"\xef\xbb\xbf<p>caf\xc3\xa9",
        );
        assert_eq!((text.as_str(), note), ("café", None));
    }

    /// Text in another encoding by its label; with none, UTF-8 when it is
    /// UTF-8, else windows-1252, as a page with no label or `<meta>`.
    #[test]
    fn text_is_decoded_by_its_label_else_falls_back() {
        let (text, note) = shown(
            "text/plain; charset=iso-8859-2",
            b"Za\xbf\xf3\xb3\xe6 g\xea\xb6l\xb1 ja\xbc\xf1",
        );
        assert_eq!(text, "Zażółć gęślą jaźń");
        assert_eq!(note.as_deref(), Some("ISO-8859-2"));
        assert_eq!(shown("text/plain", b"caf\xc3\xa9"), ("café".into(), None));
        let windows = ("café".into(), Some("windows-1252".into()));
        assert_eq!(shown("text/plain", b"caf\xe9"), windows);
        assert_eq!(shown("text/html", b"<p>caf\xe9"), windows);
        assert_eq!(shown("text/html", b"<p>caf\xc3\xa9").1, None);
        // XML's default is UTF-8, not windows-1252.
        assert_eq!(
            shown("application/xhtml+xml", b"<p>caf\xe9"),
            ("caf\u{fffd}".into(), None)
        );
    }

    /// Malformed bytes are replacements and the page renders on.
    #[test]
    fn malformed_bytes_are_replaced_and_the_page_renders() {
        let (text, _) = shown("text/html; charset=utf-8", b"<p>a\xffb</p><p>\xe2\x82 next");
        assert_eq!(text, "a\u{fffd}b\n\u{fffd} next");
        let (text, note) = shown("text/html; charset=shift_jis", b"<p>x\x93\xff y\x93");
        assert_eq!(text, "x\u{fffd} y\u{fffd}");
        assert_eq!(note.as_deref(), Some("Shift_JIS"));
        let (text, _) = shown("text/plain; charset=utf-16le", b"o\x00k\x00!");
        assert_eq!(text, "ok\u{fffd}");
    }

    /// The bounds are on the text decoded, in UTF-8: markup decoded three
    /// bytes a byte reaches `MAX_MARKUP` from a third of as many bytes;
    /// text likewise at `MAX_TEXT`.
    #[test]
    fn a_page_that_expands_is_cut_at_its_bound_in_utf8() {
        let size = MAX_MARKUP / 2;
        let mut page = b"<!--".to_vec();
        page.resize(size, 0x80);
        let doc = reply(200, Some("text/html; charset=windows-1252"), &page);
        let read = 4 + (MAX_MARKUP - 4) / 3;
        assert_eq!(
            doc.note.as_deref().unwrap(),
            format!("windows-1252 · only the first {read} of the page's {size} bytes are shown")
        );
        assert!(doc.page().is_some());
        // Halfwidth katakana, each three bytes of UTF-8.
        let size = MAX_TEXT / 3 + 10;
        let doc = reply(
            200,
            Some("text/plain; charset=shift_jis"),
            &vec![0xb1; size],
        );
        let read = (MAX_TEXT - crate::tree::TEXT_PREFIX.len()) / 3;
        assert_eq!(
            doc.note.as_deref().unwrap(),
            format!("Shift_JIS · only the first {read} of the page's {size} bytes are shown")
        );
        assert!(doc.page().is_some());
        // Text within its prefix's length of the bound is cut, and said.
        let size = MAX_TEXT - 10;
        let doc = reply(200, Some("text/plain"), &vec![b'a'; size]);
        let read = MAX_TEXT - crate::tree::TEXT_PREFIX.len();
        assert_eq!(
            doc.note.as_deref().unwrap(),
            format!("only the first {read} of the page's {size} bytes are shown")
        );
    }

    /// Markup past `MAX_MARKUP` is parsed to it, and the status row says
    /// how much of it was shown.
    #[test]
    fn a_page_past_the_markup_bound_notes_how_much_was_shown() {
        let mut page = b"<p>shown<!--".to_vec();
        page.resize(MAX_MARKUP + 10, b'x');
        let size = page.len();
        let doc = html_doc(&page);
        assert_eq!(
            doc.note.as_deref(),
            Some(
                format!("only the first {MAX_MARKUP} of the page's {size} bytes are shown")
                    .as_str()
            )
        );
        assert_eq!(text_of(&doc), "shown");
        let doc = html_doc(b"<p>whole");
        assert!(doc.note.is_none());
    }

    #[test]
    fn another_type_is_named_and_not_shown() {
        let mut doc = reply(200, Some("application/pdf"), &[1; 10]);
        let text = text_of(&doc);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "This is application/pdf, 10 bytes.");
        assert_eq!(lines[1], "Ctrl+S saves it in Downloads.");
        assert!(lines[2].contains("Firefox opens the rest"), "{lines:?}");
        assert_eq!(doc.page().unwrap().tree.source(), Source::Own);
        assert_eq!(
            doc.take_held().as_deref().map(Vec::as_slice),
            Some(&[1u8; 10][..]),
            "kept for a save"
        );
        assert!(doc.take_held().is_none(), "given up once");
        // What the server answered without success is neither offered nor
        // kept; nor is a page td-web shows.
        let mut doc = reply(404, Some("application/pdf"), &[1; 10]);
        let text = text_of(&doc);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains("Firefox opens the rest"), "{lines:?}");
        assert!(doc.take_held().is_none());
        let mut doc = reply(200, Some("text/plain"), b"x");
        assert!(doc.take_held().is_none());
        // A type's name shows its controls encoded.
        let doc = reply(200, Some("application/x\x01y"), &[1]);
        assert!(text_of(&doc).starts_with("This is application/x%01y, 1 bytes."));
    }

    #[test]
    fn an_error_status_with_nothing_to_show_is_an_error_page() {
        let doc = reply(404, Some("text/html"), b"");
        assert_eq!(doc.status, 404);
        assert_eq!(
            text_of(&doc),
            "The server answered 404 and sent nothing to show."
        );
        let doc = reply(404, Some("text/html"), b"<p>gone");
        assert_eq!(text_of(&doc), "gone");
    }

    /// The title is the tree's first HTML `title` element's text, its
    /// white space collapsed: not text that looks like one inside a
    /// script, a comment or another element, nor an SVG `title`.
    #[test]
    fn a_title_is_found_only_as_a_title() {
        let title = |markup: &str| html_doc(markup.as_bytes()).title.clone();
        assert_eq!(
            title("<titlebar>x</titlebar><title>y</title>"),
            Some("y".into())
        );
        assert_eq!(
            title("<TITLE lang=en>\n Two\n words </TITLE>"),
            Some("Two words".into())
        );
        assert_eq!(
            title("<title> A &amp; B </title><title>second</title>"),
            Some("A & B".into())
        );
        assert_eq!(title("<title></title>"), None);
        assert_eq!(title("<title>  </title>"), None);
        assert_eq!(title("<title>unclosed"), Some("unclosed".into()));
        assert_eq!(
            title("<script>'<title>no</title>'</script><title>yes</title>"),
            Some("yes".into())
        );
        assert_eq!(title("<!-- <title>no</title> --><p>x"), None);
        assert_eq!(title("<svg><title>no</title></svg><p>x"), None);
        // A text page has none, whatever its text says.
        assert!(reply(200, Some("text/plain"), b"<title>no</title>")
            .title
            .is_none());
    }

    #[test]
    fn a_link_shows_its_controls_encoded() {
        assert_eq!(
            visible("https://example.org/b#x\x01y\u{2028}z\u{85}"),
            "https://example.org/b#x%01y%E2%80%A8z%C2%85"
        );
        assert_eq!(visible("plain é"), "plain é");
    }

    #[test]
    fn a_png_is_a_picture_and_one_too_large_is_refused_from_its_header() {
        let png = td_png::encode_rgb8(2, 1, &[255, 0, 0, 0, 255, 0]).unwrap();
        let doc = reply(200, Some("image/png"), &png);
        let shown = picture(&doc).unwrap();
        assert_eq!((shown.width(), shown.height()), (2, 1));
        assert!(doc.page().is_none());
        let wide = td_png::encode_rgb8(
            MAX_PICTURE_AXIS + 1,
            1,
            &vec![0; (MAX_PICTURE_AXIS + 1) * 3],
        )
        .unwrap();
        let doc = reply(200, Some("image/png"), &wide);
        assert!(picture(&doc).is_none());
        assert!(text_of(&doc).contains("too large to show"));
        let doc = reply(200, Some("image/png"), b"not a png");
        assert!(picture(&doc).is_none());
        assert!(text_of(&doc).starts_with("The PNG could not be read"));
    }

    #[test]
    fn a_jpeg_is_a_picture_in_rgba() {
        let mut encoder = td_jpeg::Encoder::new(8, 8, 90).unwrap();
        encoder
            .encode_rows(&[200; 8 * 8 * 3], &td_jpeg::InOrder)
            .unwrap();
        let jpeg = encoder.finish(&td_jpeg::InOrder).unwrap();
        let doc = reply(200, None, &jpeg);
        let shown = picture(&doc).unwrap();
        assert_eq!((shown.width(), shown.height()), (8, 8));
        assert_eq!(shown.bytes(), 8 * 8 * 4);
    }

    #[test]
    fn gif_mime_sniffing_and_errors_use_the_same_picture_path() {
        for version in [b"GIF87a", b"GIF89a"] {
            let mut gif = picture::tests::GIF.to_vec();
            gif[..6].copy_from_slice(version);
            for media in [
                None,
                Some("application/octet-stream"),
                Some("IMAGE/GIF; x=y"),
            ] {
                let doc = reply(200, media, &gif);
                let fitted = picture(&doc).unwrap().fit(2, 1).unwrap();
                assert_eq!(fitted.pixel(0, 0), Some([0, 0, 0, 0]));
                assert_eq!(fitted.pixel(1, 0), Some([0, 0, 255, 255]));
            }
        }
        let doc = reply(200, Some("application/pdf"), picture::tests::GIF);
        assert!(picture(&doc).is_none());
        let doc = reply(200, Some("image/gif"), b"not GIF");
        assert!(picture(&doc).is_none());
        assert!(text_of(&doc).starts_with("The GIF could not be read"));
    }

    #[test]
    fn avif_navigation_uses_the_shared_decoder_and_admitted_still_subset() {
        let expected = Picture::new(6, 4, [232, 134, 109, 255].repeat(24)).unwrap();
        for mime in [
            Some("image/avif"),
            Some("IMAGE/AVIF; x=y"),
            None,
            Some("application/octet-stream"),
        ] {
            let doc = reply(200, mime, picture::tests::AVIF);
            assert_eq!(picture(&doc).unwrap().fit(6, 4), expected.fit(6, 4));
        }
        let doc = reply(200, Some("application/pdf"), picture::tests::AVIF);
        assert!(picture(&doc).is_none());
        let doc = reply(
            200,
            Some("image/avif"),
            include_bytes!("../../td-avif/tests/fixtures/libavif.avif"),
        );
        assert!(picture(&doc).is_none());
        assert!(text_of(&doc).starts_with("The AVIF could not be decoded"));
    }

    #[test]
    fn webp_navigation_uses_mime_or_container_signature() {
        let expected = Picture::new(
            35,
            27,
            include_bytes!("../../td-webp/tests/fixtures/palette.rgba").to_vec(),
        )
        .unwrap();
        for mime in [
            Some("image/webp"),
            Some("IMAGE/WEBP; x=y"),
            None,
            Some("application/octet-stream"),
        ] {
            let doc = reply(200, mime, picture::tests::WEBP);
            assert_eq!(picture(&doc).unwrap().fit(35, 27), expected.fit(35, 27));
        }
        let doc = reply(200, Some("application/pdf"), picture::tests::WEBP);
        assert!(picture(&doc).is_none());
    }

    #[test]
    fn fitted_pixels_outlive_their_document_and_refusals_release_credit() {
        // Decoding needs its working allowance and control blocks, not
        // an extra allowance for an error notice that success never uses.
        let root = Budget::new(memory::PICTURE_WORK + 2 * Budget::metadata_bytes()).unwrap();
        let baseline = root.used();
        let doc = Document::from_reply_in(
            base(),
            200,
            Some("image/gif"),
            picture::tests::GIF.to_vec(),
            WANT,
            None,
            &AtomicBool::new(false),
            &root,
        )
        .unwrap();
        let pixels = root.used();
        assert!(td_alloc::with_reservation_limit(0, || doc.fit_picture(4, 4)).is_none());
        assert_eq!(root.used(), pixels);
        let fit = doc.fit_picture(4, 4).unwrap();
        let bytes = fit.bgra.capacity();
        assert_eq!(root.used(), pixels + bytes);
        drop(doc);
        assert_eq!(root.used(), baseline + Budget::metadata_bytes() + bytes);
        assert!(!fit.bgra.is_empty());
        drop(fit);
        assert_eq!(root.used(), baseline);
    }

    #[test]
    fn resource_failure_document_needs_no_error_page() {
        let doc = Document::resources(None, 200);
        assert!(resources(&doc));
        assert!(picture(&doc).is_none());
        assert!(doc.page().is_none());
        assert!(doc.fit_picture(4, 4).is_none());
        // A root with no room left gives the notice, not a page.
        let root = memory::root().unwrap();
        let _blocker = root.charge(root.limit() - root.used()).unwrap();
        let doc = Document::own(None, 0, "an error", WANT, None, &root);
        assert!(resources(&doc));
    }

    #[test]
    fn page_root_refusal_preserves_the_displayed_page_and_held_save_charge() {
        let root = memory::root().unwrap();
        let baseline = root.used();
        let made = |content_type: &str, body: &[u8]| {
            Document::from_reply_in(
                base(),
                200,
                Some(content_type),
                body.to_vec(),
                WANT,
                None,
                &AtomicBool::new(false),
                &root,
            )
            .unwrap()
        };
        let old = made("text/plain", b"old page");
        let kept = root.used();
        let blocker = root.charge(root.limit() - root.used()).unwrap();
        let refused = made("text/html", b"<p>new page");
        assert!(resources(&refused));
        drop((refused, blocker));
        assert_eq!(root.used(), kept);
        assert_eq!(text_of(&old), "old page");
        drop(old);
        assert_eq!(root.used(), baseline);
        let mut held = made("application/pdf", &[9; 4096]);
        let body = held.take_held().unwrap();
        drop(held);
        assert_eq!(
            root.used(),
            baseline + Budget::metadata_bytes() + body.capacity()
        );
        drop(body);
        assert_eq!(root.used(), baseline);
    }

    /// The limits as measured against the jail (DESIGN.md §3 and §10):
    /// decoded text takes its body's place, so is no more than a body
    /// could be; the costliest picture decoding (a 16-bit RGBA PNG of
    /// `MAX_PICTURE_PIXELS` inflated at 8 bytes a pixel beside its 4-byte
    /// RGBA, 201 MB measured) and every decoder's bulk fit its working
    /// allowance; and a picture beside its fitted copy fits the root.
    /// `the_page_budgets_fit_the_jail` (memory.rs) holds the root itself
    /// to the jail.
    #[test]
    fn page_admission_and_decoder_allowances_fit_the_jail() {
        const DOCUMENT: usize = crate::fetch::MAX_DOCUMENT as usize;
        const ROWS: usize = 16 * MAX_PICTURE_AXIS;
        const {
            assert!(MAX_MARKUP <= DOCUMENT && MAX_TEXT <= DOCUMENT);
            assert!(memory::PICTURE_WORK >= (192 << 20) + (1 << 20));
            assert!(memory::PICTURE_WORK >= (16 << 20) + 4 * MAX_PICTURE_PIXELS + ROWS);
            assert!(memory::PICTURE_WORK >= 12 * MAX_PICTURE_PIXELS + ROWS + (1 << 20));
            assert!(memory::PICTURE_WORK >= 2 * DOCUMENT + 8 * MAX_PICTURE_PIXELS + ROWS);
            assert!(memory::PICTURE_WORK >= 3 * DOCUMENT);
            assert!(2 * 4 * MAX_PICTURE_PIXELS <= memory::PAGES);
        }
        let root = memory::root().unwrap();
        assert_eq!(root.limit(), memory::PAGES + 16 * Budget::metadata_bytes());
        assert!(memory::PICTURE_WORK <= root.limit());
    }

    #[test]
    fn a_stopped_navigation_renders_and_decodes_nothing() {
        let root = memory::root().unwrap();
        let baseline = root.used();
        let stopped = AtomicBool::new(true);
        for (kind, body) in [
            ("text/html", b"<p>x".to_vec()),
            ("text/plain", b"x".to_vec()),
            ("application/pdf", b"x".to_vec()),
            ("image/png", td_png::encode_rgb8(1, 1, &[0, 0, 0]).unwrap()),
            ("image/gif", picture::tests::GIF.to_vec()),
            ("image/webp", picture::tests::WEBP.to_vec()),
            ("image/avif", picture::tests::AVIF.to_vec()),
        ] {
            let doc =
                Document::from_reply_in(base(), 200, Some(kind), body, WANT, None, &stopped, &root);
            assert!(doc.is_none(), "{kind}");
            assert_eq!(root.used(), baseline, "{kind}");
        }
    }

    /// Replies handed over from several fetch threads at once each come
    /// back as their own document, made on the render thread, and one
    /// whose navigation was stopped is not rendered, or not kept.
    #[test]
    fn the_renderer_answers_each_reply_and_renders_no_stopped_one() {
        let renderer = Renderer::start(None).unwrap();
        let threads: Vec<_> = (0..4)
            .map(|n| {
                let renderer = renderer.clone();
                std::thread::spawn(move || {
                    let body = format!("<title>page {n}</title><p>text {n}").into_bytes();
                    let going = Arc::new(AtomicBool::new(false));
                    let doc = renderer.document(base(), 200, Some("text/html"), body, WANT, &going);
                    (n, doc.unwrap())
                })
            })
            .collect();
        for thread in threads {
            let (n, doc) = thread.join().unwrap();
            assert_eq!(doc.title, Some(format!("page {n}")));
            assert_eq!(text_of(&doc), format!("text {n}"));
        }
        let stopped = Arc::new(AtomicBool::new(true));
        let body = b"<p>x".to_vec();
        let doc = renderer.document(base(), 200, Some("text/html"), body, WANT, &stopped);
        assert!(doc.is_none());
        // Already stopped text is skipped before allocating a handoff too.
        let body = b"x".to_vec();
        let doc = renderer.document(base(), 200, Some("text/plain"), body, WANT, &stopped);
        assert!(doc.is_none());
        // An error page is laid on the calling thread.
        let doc = renderer.error(Some(base()), "nothing here", WANT);
        assert_eq!(doc.status, 0);
        assert_eq!(text_of(&doc), "nothing here");
    }

    #[test]
    fn renderer_handoff_refusal_keeps_the_old_page_and_returns_credit() {
        let root = memory::root().unwrap();
        let renderer = Renderer::with_budget(&root, None).unwrap();
        let going = Arc::new(AtomicBool::new(false));
        let old = renderer
            .document(
                base(),
                200,
                Some("text/plain"),
                b"old".to_vec(),
                WANT,
                &going,
            )
            .unwrap();
        renderer.idle();
        let baseline = root.used();
        let long = format!("text/plain; x={}", "x".repeat(128));
        for (kind, needed) in [("text/plain", 0), (long.as_str(), 1)] {
            for allowance in 0..=needed {
                let url = base();
                let body = b"new".to_vec();
                let doc = td_alloc::with_reservation_limit(allowance, || {
                    renderer.document(url, 200, Some(kind), body, WANT, &going)
                })
                .unwrap();
                assert_eq!(resources(&doc), allowance < needed);
                drop(doc);
                renderer.idle();
                assert_eq!(root.used(), baseline);
                assert_eq!(text_of(&old), "old");
            }
        }
        let blocker = root.charge(root.limit() - root.used()).unwrap();
        let refused = renderer
            .document(base(), 200, None, b"new".to_vec(), WANT, &going)
            .unwrap();
        assert!(resources(&refused));
        going.store(true, Ordering::SeqCst);
        assert!(renderer
            .document(
                base(),
                200,
                Some("text/plain"),
                b"stopped".to_vec(),
                WANT,
                &going
            )
            .is_none());
        drop((blocker, refused, old, renderer));
        assert_eq!(root.used(), Budget::metadata_bytes());
    }

    #[test]
    fn handoff_headers_keep_every_byte_across_inline_boundary() {
        let root = memory::root().unwrap();
        let baseline = root.used();
        for len in [0, 127, 128, 129, 8192] {
            let text = "x".repeat(len);
            let kind = ContentType::copy(&root, &text).unwrap();
            assert_eq!(kind.as_str(), text);
            assert_eq!(root.used() == baseline, len <= 128);
            drop(kind);
            assert_eq!(root.used(), baseline);
        }
        let text = format!("{}é", "x".repeat(127));
        assert_eq!(ContentType::copy(&root, &text).unwrap().as_str(), text);
        assert_eq!(crate::worker::QUEUED, crate::app::MAX_WORKERS);
    }

    #[test]
    fn a_renderer_restarts_a_lost_worker() {
        let renderer = Renderer::start(None).unwrap();
        renderer.0.thread.lock().unwrap().stop();
        let going = Arc::new(AtomicBool::new(false));
        for _ in 0..2 {
            let doc = renderer.document(
                base(),
                200,
                Some("text/html"),
                b"<p>still here".to_vec(),
                WANT,
                &going,
            );
            assert_eq!(text_of(&doc.unwrap()), "still here");
        }
    }

    #[test]
    fn cancelled_render_jobs_return_their_page_admission() {
        let root = memory::root().unwrap();
        let renderer = Renderer::with_budget(&root, None).unwrap();
        let baseline = root.used();
        let stopped = Arc::new(AtomicBool::new(true));
        for kind in ["text/html", "text/plain", "image/gif"] {
            assert!(renderer
                .document(
                    base(),
                    200,
                    Some(kind),
                    b"<p>cancelled".to_vec(),
                    WANT,
                    &stopped
                )
                .is_none());
            renderer.idle();
            assert_eq!(root.used(), baseline);
        }
        stopped.store(false, Ordering::SeqCst);
        let doc = renderer
            .document(
                base(),
                200,
                Some("text/plain"),
                b"kept".to_vec(),
                WANT,
                &stopped,
            )
            .unwrap();
        drop(renderer);
        assert!(root.used() > Budget::metadata_bytes());
        drop(doc);
        assert_eq!(root.used(), Budget::metadata_bytes());
    }

    /// What a relayout of `page` for `want` is given, its old layout kept
    /// beside it.
    pub(crate) fn again_of(page: &Page, want: Want) -> Again {
        Again {
            tree: page.tree.try_clone().unwrap(),
            markup: page.markup.as_ref().map(|m| m.try_clone().unwrap()),
            rung: page.rung,
            notes: page.notes,
            budget: page.budget.try_clone().unwrap(),
            old: page.laid.as_ref().map(|laid| laid.try_clone().unwrap()),
            keep: true,
            want,
        }
    }

    /// A relayout whose job panics on the lane is answered refused, by
    /// the guard the lane holds, not left unanswered.
    #[test]
    fn a_relayout_whose_job_panics_is_answered_refused() {
        let root = memory::root().unwrap();
        let renderer = Renderer::with_budget(&root, None).unwrap();
        let going = Arc::new(AtomicBool::new(false));
        let doc = renderer
            .document(
                base(),
                200,
                Some("text/html"),
                b"<p>a".to_vec(),
                WANT,
                &going,
            )
            .unwrap();
        let page = doc.page().unwrap();
        let (faces, cancel, answer) = renderer.relayout_parts().unwrap();
        let want = Want {
            width: PANICS,
            ..WANT
        };
        let posted = renderer.post(Lane::Again {
            again: again_of(page, want),
            faces,
            cancel,
            answer: answer.try_clone().unwrap(),
        });
        assert!(posted.is_ok());
        renderer.idle();
        assert!(matches!(
            answer.lock().unwrap().take(),
            Some(Err(Refusal::Limit(_)))
        ));
    }

    /// A relayout cancelled after its page was made, before the lane
    /// answers, is answered cancelled, the page dropped there.
    #[test]
    fn a_relayout_cancelled_once_made_is_answered_cancelled() {
        let renderer = Renderer::start(None).unwrap();
        let going = Arc::new(AtomicBool::new(false));
        let doc = renderer
            .document(
                base(),
                200,
                Some("text/html"),
                b"<p>a".to_vec(),
                WANT,
                &going,
            )
            .unwrap();
        let page = doc.page().unwrap();
        let (faces, cancel, answer) = renderer.relayout_parts().unwrap();
        let want = Want {
            width: CANCELLED,
            ..WANT
        };
        let posted = renderer.post(Lane::Again {
            again: again_of(page, want),
            faces,
            cancel: cancel.try_clone().unwrap(),
            answer: answer.try_clone().unwrap(),
        });
        assert!(posted.is_ok());
        renderer.idle();
        assert!(cancel.load(Ordering::SeqCst));
        assert!(matches!(
            answer.lock().unwrap().take(),
            Some(Err(Refusal::Cancelled))
        ));
    }

    /// A relayout posted to the lane is laid on the render thread and
    /// answered through its slot; a cancelled one answers so.
    #[test]
    fn a_relayout_through_the_lane_answers_through_its_slot() {
        let renderer = Renderer::start(None).unwrap();
        let going = Arc::new(AtomicBool::new(false));
        let doc = renderer
            .document(
                base(),
                200,
                Some("text/html"),
                b"<p>alpha beta gamma delta".to_vec(),
                WANT,
                &going,
            )
            .unwrap();
        let page = doc.page().unwrap();
        let narrow = Want {
            width: 160.0,
            ..WANT
        };
        let (faces, cancel, answer) = renderer.relayout_parts().unwrap();
        let posted = renderer.post(Lane::Again {
            again: again_of(page, narrow),
            faces,
            cancel,
            answer: answer.try_clone().unwrap(),
        });
        assert!(posted.is_ok());
        renderer.idle();
        let laid = answer.lock().unwrap().take().unwrap().unwrap();
        assert_eq!(laid.rung, Rung::Own);
        let new = laid.laid.as_ref().unwrap();
        assert_eq!(new.want, narrow);
        let old = page.laid.as_ref().unwrap();
        assert!(new.lines().len() > old.lines().len());
        assert_eq!(copy_of(new), copy_of(old));
        let (faces, cancel, answer) = renderer.relayout_parts().unwrap();
        cancel.store(true, Ordering::SeqCst);
        let posted = renderer.post(Lane::Again {
            again: again_of(page, narrow),
            faces,
            cancel,
            answer: answer.try_clone().unwrap(),
        });
        assert!(posted.is_ok());
        renderer.idle();
        assert_eq!(
            answer.lock().unwrap().take().unwrap().err(),
            Some(Refusal::Cancelled)
        );
    }

    /// A document and a layout let go are dropped on the render thread,
    /// their charges returned to the page root.
    #[test]
    fn a_document_let_go_returns_its_page_charge() {
        let root = memory::root().unwrap();
        let renderer = Renderer::with_budget(&root, None).unwrap();
        renderer.idle();
        let baseline = root.used();
        let going = Arc::new(AtomicBool::new(false));
        let doc = renderer
            .document(
                base(),
                200,
                Some("text/html"),
                b"<p>let go".to_vec(),
                WANT,
                &going,
            )
            .unwrap();
        renderer.idle();
        assert!(root.used() > baseline);
        let (told, dropped) = std::sync::mpsc::channel();
        let mut doc = doc;
        doc.recorder = Some(told);
        renderer.let_go(Dropped::Document(doc));
        renderer.idle();
        assert_ne!(dropped.recv().unwrap(), std::thread::current().id());
        assert_eq!(root.used(), baseline);
    }
}
