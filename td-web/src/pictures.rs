//! A page's inline pictures (DESIGN.md §10, "Subresources" and "Slots"):
//! each `img` with a `src`, in tree order, at most `MAX_PICTURES`, fetched
//! once the page is shown within the window's subresource slots, each on
//! a thread of its own that hands its body to the render thread; decoded
//! there under the page's pictures term; and taken by the window, which
//! lays the page out again for those come, at most once every
//! `PICTURE_SETTLE`. A decoded picture's last drop is the render
//! thread's, as a page's is.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::document::{Lane, Renderer};
use crate::fetch::{self, Ask, Failure, Loaded};
use crate::picture::{self, Format};
use crate::subresource::{Slot, Slots};
use td_alloc::{Budget, BudgetError, BudgetShared, BudgetString, BudgetVec};
use td_html::document::NodeId;
use td_page::inline::{Inline, Set, Witness, MAX_PICTURES};
use td_page::tree::Tree;
use td_url::{Reference, Url};
/// How often, at most, pictures that came lay the page out again, in ms.
pub const PICTURE_SETTLE: u64 = 250;
/// How long a picture's fetch is given, each hop asked within it, so a
/// page's pictures hold the subresource slots no longer.
pub const PICTURE_WAIT: Duration = Duration::from_secs(30);
/// The most of the window's `MAX_SUBRESOURCES` slots pictures take at
/// once: the rest stay free for the next page's sheets, which hold it
/// back from being shown.
pub const PICTURE_SLOTS: usize = 2;
/// The most overlap steps one layout's fitted copies take
/// (`Fitted::fit_exact_cost`): a copy past them keeps its placeholder,
/// so a layout's fitting is bounded whatever its pictures and boxes.
pub const FIT_WORK: u64 = 32 << 20;
/// The most bytes of a picture's body, asked of the service as its limit.
pub const MAX_PICTURE_BODY: u64 = 4 << 20;
/// What a picture asks for: the formats td-web decodes, AVIF unnamed while
/// its coverage is partial (DESIGN.md §5), else anything, sniffed.
pub const ACCEPT: &str = "image/png,image/jpeg,image/gif,image/webp,*/*;q=0.1";
/// What a picture asks: `public` whatever its page's reach; its deadline
/// is set as it is asked.
const PICTURE: Ask = Ask {
    public: true,
    accept: ACCEPT,
    limit: MAX_PICTURE_BODY,
    deadline: None,
};

const _: () = assert!(PICTURE_SLOTS <= crate::subresource::MAX_SUBRESOURCES);

/// Tells, in tests, what of a page's pictures was dropped and whether on
/// the render thread.
#[cfg(test)]
pub(crate) type Told = std::sync::mpsc::Sender<(&'static str, bool)>;

#[cfg(test)]
fn tell(told: Option<&Told>, what: &'static str) {
    if let Some(told) = told {
        let render = std::thread::current().name() == Some("td-web render");
        let _ = told.send((what, render));
    }
}

/// Why a picture is not shown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Why {
    /// Its fetch failed or was refused: an answer other than `2xx`, mixed
    /// content, too many redirects, a body past `MAX_PICTURE_BODY`, past
    /// `PICTURE_WAIT`; or what came is no picture td-web decodes.
    Failed,
    /// The pictures term had not the room to decode or keep it.
    Room,
    /// Not fetched: SVG, a URL td-web does not take, a `data:` URL that
    /// does not decode.
    Skipped,
    /// `Escape` stopped it before it came.
    Stopped,
}

/// What became of a picture asked for.
pub enum Outcome {
    Shown(BudgetShared<Inline>),
    Refused(Why),
}

/// A picture's place in its page's inbox: each picture has its own, so
/// no answer waits for another's to be taken.
enum Place {
    Empty,
    Ready(Outcome),
    Taken,
}

/// Where a page's picture fetches and decodings answer, and whether the
/// page has gone, which each reads before each hop and before it puts.
pub struct Inbox {
    places: Mutex<[Place; MAX_PICTURES]>,
    gone: AtomicBool,
    #[cfg(test)]
    told: Option<Told>,
    #[cfg(test)]
    witness: Option<Witness>,
}

impl Inbox {
    /// Told where a page's decoded pictures and their copies drop, in
    /// tests.
    fn witness(&self) -> Option<Witness> {
        #[cfg(test)]
        return self.witness;
        #[cfg(not(test))]
        None
    }

    /// `outcome` for picture `at`, given back when the page has gone or
    /// the picture was answered already, for the caller to let go.
    fn put(&self, at: usize, outcome: Outcome) -> Option<Outcome> {
        let mut places = self.places.lock().unwrap_or_else(PoisonError::into_inner);
        if self.gone.load(Ordering::SeqCst) {
            return Some(outcome);
        }
        match places.get_mut(at) {
            Some(place @ Place::Empty) => {
                *place = Place::Ready(outcome);
                None
            }
            _ => Some(outcome),
        }
    }

    /// Marks the page gone, under the lock `put` reads it under: no
    /// answer is put after it. What it holds stays, for `clear`.
    fn close(&self) {
        let _places = self.places.lock().unwrap_or_else(PoisonError::into_inner);
        self.gone.store(true, Ordering::SeqCst);
    }

    /// Closes it and lets go of the answers it still holds, here: on the
    /// render thread, as its page's pictures are let go.
    fn clear(&self) {
        let mut places = self.places.lock().unwrap_or_else(PoisonError::into_inner);
        self.gone.store(true, Ordering::SeqCst);
        let left = std::mem::replace(&mut *places, std::array::from_fn(|_| Place::Taken));
        drop(places);
        drop(left);
    }
}

/// Moves the answers ready in `places` into `entries`, those shown
/// numbered from `came` in the order taken; whether any was.
fn take_into(places: &mut [Place], entries: &mut [Entry], came: &mut u32) -> bool {
    let mut any = false;
    for (place, entry) in places.iter_mut().zip(entries.iter_mut()) {
        if !matches!(place, Place::Ready(_)) {
            continue;
        }
        let Place::Ready(outcome) = std::mem::replace(place, Place::Taken) else {
            continue;
        };
        any = true;
        entry.state = match outcome {
            Outcome::Shown(inline) => {
                *came = came.saturating_add(1);
                State::Shown {
                    inline,
                    came: *came,
                }
            }
            Outcome::Refused(why) => State::Refused(why),
        };
    }
    any
}

/// A picture of the page, by its place in the tree's list.
enum State {
    Wanted,
    Asked,
    /// Come: taken in this order, so a relayout lays those come by its
    /// settle.
    Shown {
        inline: BudgetShared<Inline>,
        came: u32,
    },
    Refused(Why),
}

struct Entry {
    node: NodeId,
    state: State,
}

/// The window's pictures of the page it shows.
pub struct Pictures {
    entries: BudgetVec<Entry>,
    inbox: BudgetShared<Inbox>,
    /// The pictures term: pixels, fitted copies, decoding's peak, and
    /// this bookkeeping.
    term: Budget,
    /// The page is `https`: a picture's `http` hop under it is mixed
    /// content.
    https: bool,
    /// The overlap steps a layout's copies may take.
    work: u64,
    /// The next entry to ask for.
    next: usize,
    /// How many have come; how many of them were laid by the last
    /// settle; and when the last relayout came, by the window's clock.
    came: u32,
    settled: u32,
    laid_at: Option<u64>,
    /// `img`s with a `src` past `MAX_PICTURES`.
    past: usize,
    /// The `data:` picture last handed to the render thread.
    data_at: Option<usize>,
}

/// What a picture's fetch is given and asks with.
pub struct Asker<'a> {
    pub fetcher: &'a fetch::Shared,
    pub renderer: &'a Renderer,
    pub slots: &'a Slots,
    /// The page's tree, which a `data:` picture is decoded from.
    pub tree: &'a BudgetShared<Tree>,
    /// What the page's links and pictures are joined against.
    pub base: &'a Url,
    /// The most slots pictures take at once: `PICTURE_SLOTS`.
    pub most: usize,
    /// How long each picture's fetch is given: `PICTURE_WAIT`.
    pub wait: Duration,
}

/// How many of a page's pictures are shown, of how many, and whether any
/// is still to come.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Count {
    pub shown: usize,
    pub of: usize,
    pub not: usize,
    pub out: bool,
    /// `Escape` stopped some: a reload asks for them again.
    pub stopped: bool,
}

impl Pictures {
    /// The pictures `tree` names, a page from `url` made in `page`, its
    /// term a child of it of `term` bytes; none when it names none.
    pub fn new(
        tree: &Tree,
        url: &Url,
        page: &Budget,
        term: usize,
        work: u64,
    ) -> Result<Option<Self>, BudgetError> {
        let nodes = tree.pictures();
        if nodes.is_empty() {
            return Ok(None);
        }
        let term = page.child(term.saturating_add(Budget::metadata_bytes()))?;
        let mut entries = td_alloc::BudgetVec::with_capacity(&term, nodes.len())?;
        for &node in nodes {
            entries.try_push(Entry {
                node,
                state: State::Wanted,
            })?;
        }
        let inbox = Inbox {
            places: Mutex::new(std::array::from_fn(|_| Place::Empty)),
            gone: AtomicBool::new(false),
            #[cfg(test)]
            told: tests::named(url),
            #[cfg(test)]
            witness: tests::witness(url),
        };
        Ok(Some(Self {
            entries,
            inbox: BudgetShared::new(&term, inbox)?,
            term,
            https: url.https,
            work,
            next: 0,
            came: 0,
            settled: 0,
            laid_at: None,
            past: tree.past(),
            data_at: None,
        }))
    }

    /// Takes what has come into the inbox; whether anything had.
    pub fn take(&mut self) -> bool {
        let mut places = self
            .inbox
            .places
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        take_into(places.as_mut_slice(), &mut self.entries, &mut self.came)
    }

    /// Asks for the pictures wanted, in tree order, while slots last.
    pub fn ask(&mut self, asker: &Asker<'_>) {
        while let Some(entry) = self.entries.get(self.next) {
            if !matches!(entry.state, State::Wanted) {
                self.next += 1;
                continue;
            }
            let at = self.next;
            let node = entry.node;
            let Some(src) = asker.tree.src(node) else {
                self.refuse(at, Why::Skipped);
                continue;
            };
            // A `data:` picture before it is joined, whatever its length;
            // one on the lane at a time.
            if is_data(src) {
                if self.data_out() || !self.data(asker, at, node) {
                    return;
                }
                continue;
            }
            // No slot: nothing joined or copied until one frees.
            if asker.slots.out() >= asker.most {
                return;
            }
            let url = match td_url::resolve(asker.base, src) {
                Ok(Reference::Fetch { url, .. }) => url,
                Ok(Reference::Other { .. }) | Err(_) => {
                    self.refuse(at, Why::Skipped);
                    continue;
                }
            };
            let text = url.text();
            drop(url);
            if svg(&text) {
                self.refuse(at, Why::Skipped);
                continue;
            }
            let Some(slot) = asker.slots.take_within(asker.most) else {
                return;
            };
            let Some(deadline) = Instant::now().checked_add(asker.wait) else {
                self.refuse(at, Why::Failed);
                continue;
            };
            let asked = (|| {
                Ok::<_, BudgetError>(Asked {
                    owed: Owed {
                        at,
                        inbox: self.inbox.try_clone()?,
                        armed: true,
                    },
                    url: BudgetString::copy(&self.term, &text)?,
                    https: self.https,
                    deadline,
                    term: self.term.try_clone()?,
                    renderer: asker.renderer.try_clone()?,
                })
            })();
            drop(text);
            let spawned = match asked {
                Ok(asked) => spawn(asker.fetcher, asked, slot),
                Err(_) => Err(()),
            };
            // A thread not started has put its picture `Failed`.
            match spawned {
                Ok(()) => self.mark(at, State::Asked),
                Err(()) => self.refuse(at, Why::Room),
            }
        }
    }

    /// Whether any is still to be asked for.
    pub fn wanted(&self) -> bool {
        self.entries
            .get(self.next..)
            .is_some_and(|rest| rest.iter().any(|e| matches!(e.state, State::Wanted)))
    }

    /// Whether a `data:` picture is on the lane or being decoded.
    fn data_out(&self) -> bool {
        self.data_at
            .and_then(|at| self.entries.get(at))
            .is_some_and(|entry| matches!(entry.state, State::Asked))
    }

    /// A `data:` picture handed to the render thread to decode, taking no
    /// slot; false when the lane is full, to be handed again.
    fn data(&mut self, asker: &Asker<'_>, at: usize, node: NodeId) -> bool {
        let data = (|| {
            Ok::<_, BudgetError>(Data {
                owed: Owed {
                    at,
                    inbox: self.inbox.try_clone()?,
                    armed: true,
                },
                tree: asker.tree.try_clone()?,
                node,
                term: self.term.try_clone()?,
            })
        })();
        let Ok(data) = data else {
            self.refuse(at, Why::Room);
            return true;
        };
        match asker.renderer.post(Lane::Picture(data)) {
            Ok(()) => {
                self.mark(at, State::Asked);
                self.data_at = Some(at);
                true
            }
            Err(Lane::Picture(data)) => {
                data.owed.disarm();
                false
            }
            Err(_) => false,
        }
    }

    fn mark(&mut self, at: usize, state: State) {
        if let Some(entry) = self.entries.get_mut(at) {
            entry.state = state;
        }
        self.next = self.next.max(at + 1);
    }

    fn refuse(&mut self, at: usize, why: Why) {
        self.mark(at, State::Refused(why));
    }

    /// Whether pictures have come since the last settle and the last
    /// relayout came at least `PICTURE_SETTLE` before `now`: the render
    /// thread is left that long between them, however long each takes.
    pub fn due(&self, now: u64) -> bool {
        self.came > self.settled
            && self
                .laid_at
                .is_none_or(|at| now.saturating_sub(at) >= PICTURE_SETTLE)
    }

    /// Those come so far laid by the next relayout.
    pub fn settle(&mut self) {
        self.settled = self.came;
    }

    /// A relayout of the page came at `now`.
    pub fn laid(&mut self, now: u64) {
        self.laid_at = Some(now);
    }

    /// How many have come by the last settle: what a layout is made for.
    pub fn settled(&self) -> u32 {
        self.settled
    }

    /// The pictures a relayout lays: those come by the last settle, by
    /// node, in `memory`.
    pub fn set(&self, memory: &Budget) -> Result<Set, BudgetError> {
        let mut shown = BudgetVec::new(memory)?;
        for entry in self.entries.iter() {
            if let State::Shown { inline, came } = &entry.state {
                if *came <= self.settled {
                    shown.try_push((entry.node.index(), inline.try_clone()?))?;
                }
            }
        }
        Set::new(shown, &self.term, self.work)
    }

    /// Whether a picture is still to be asked for or to come.
    pub fn out(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.state, State::Wanted | State::Asked))
    }

    /// Whether the window has pictures to ask for, take or lay.
    pub fn busy(&self) -> bool {
        self.out() || self.came > self.settled
    }

    /// `Escape`: those not come are let go, their fetches asking no
    /// further hop and what they answer let go where it is put; those
    /// come, taken under the same lock, are kept, so none is let go
    /// here. Whether any was still out.
    pub fn stop(&mut self) -> bool {
        if !self.out() {
            return false;
        }
        let mut places = self
            .inbox
            .places
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.inbox.gone.store(true, Ordering::SeqCst);
        take_into(places.as_mut_slice(), &mut self.entries, &mut self.came);
        drop(places);
        let mut any = false;
        for entry in self.entries.iter_mut() {
            if matches!(entry.state, State::Wanted | State::Asked) {
                entry.state = State::Refused(Why::Stopped);
                any = true;
            }
        }
        any
    }

    /// The page let go: its fetches ask no further hop and what they
    /// answer is let go where it is put. What has come stays until the
    /// pictures are dropped, on the render thread.
    pub fn leave(&self) {
        self.inbox.close();
    }

    /// What the status row says of them. A picture settled is shown
    /// unless the layout shown did not draw it (`drawn` false): its box's
    /// copy was refused. One with no box, `display: none`, counts as
    /// shown.
    pub fn count(&self, drawn: impl Fn(NodeId) -> bool) -> Count {
        let mut count = Count {
            of: self.entries.len().saturating_add(self.past),
            not: self.past,
            ..Count::default()
        };
        for entry in self.entries.iter() {
            match &entry.state {
                State::Shown { came, .. } if *came <= self.settled => {
                    if drawn(entry.node) {
                        count.shown += 1;
                    } else {
                        count.not += 1;
                    }
                }
                State::Shown { .. } | State::Wanted | State::Asked => count.out = true,
                State::Refused(why) => {
                    count.not += 1;
                    count.stopped |= *why == Why::Stopped;
                }
            }
        }
        count
    }

    /// Why picture `at` of the tree's list is not shown, if it is not.
    #[cfg(test)]
    pub fn refused(&self, at: usize) -> Option<Why> {
        match self.entries.get(at)?.state {
            State::Refused(why) => Some(why),
            _ => None,
        }
    }

    /// The pictures term.
    #[cfg(test)]
    pub fn term(&self) -> &Budget {
        &self.term
    }
}

impl Drop for Pictures {
    /// What the inbox still holds goes with the page, here: the render
    /// thread's, as the window lets its pictures go.
    fn drop(&mut self) {
        self.inbox.clear();
        #[cfg(test)]
        tell(self.inbox.told.as_ref(), "pictures");
    }
}

/// Whether a URL's path names an SVG file, which td-web does not fetch.
fn svg(text: &str) -> bool {
    let path = text.split(['?', '#']).next().unwrap_or(text);
    let tail = |suffix: &str| {
        path.len() >= suffix.len()
            && path
                .get(path.len() - suffix.len()..)
                .is_some_and(|end| end.eq_ignore_ascii_case(suffix))
    };
    tail(".svg") || tail(".svgz")
}

/// What a picture's fetch thread is given.
struct Asked {
    owed: Owed,
    url: BudgetString,
    https: bool,
    deadline: Instant,
    term: Budget,
    renderer: Renderer,
}

/// Picture `at`'s answer, owed to its inbox: put `Failed` as it drops,
/// unless it was answered first, so no picture stays out, whatever ends
/// its fetch or decoding, a panic among them.
struct Owed {
    at: usize,
    inbox: BudgetShared<Inbox>,
    armed: bool,
}

impl Owed {
    /// Owed no more: its picture is to be asked again.
    fn disarm(mut self) {
        self.armed = false;
    }

    fn gone(&self) -> bool {
        self.inbox.gone.load(Ordering::SeqCst)
    }

    /// `outcome` put; one given back, the page gone or the picture
    /// answered, is let go here.
    fn answer(&self, outcome: Outcome) {
        drop(self.inbox.put(self.at, outcome));
    }
}

impl Drop for Owed {
    fn drop(&mut self) {
        if self.armed {
            self.answer(Outcome::Refused(Why::Failed));
        }
    }
}

/// Fetches a picture on a thread of its own, holding `slot` until the
/// render thread has decoded it and let its body go, or it was refused.
fn spawn(fetcher: &fetch::Shared, asked: Asked, slot: Slot) -> Result<(), ()> {
    let fetcher = fetch::Shared::clone(fetcher);
    let builder = match td_alloc::string("td-web picture") {
        Ok(name) => std::thread::Builder::new().name(name),
        Err(_) => std::thread::Builder::new(),
    };
    builder
        .spawn(move || {
            let Asked {
                owed,
                url,
                https,
                deadline,
                term,
                renderer,
            } = asked;
            let ask = Ask {
                deadline: Some(deadline),
                ..PICTURE
            };
            let fetched = match Url::parse(url.as_str()) {
                Ok(url) => fetch::subresource(fetcher.as_ref(), url, https, ask, &owed.inbox.gone),
                Err(_) => Err(Failure::Fetch),
            };
            drop(url);
            // Its page gone, past its wait, or anything but `2xx`: failed,
            // as `owed` drops.
            if let Ok(loaded) = fetched {
                if (200..300).contains(&loaded.reply.status) {
                    renderer.picture(Decode { owed, loaded, term });
                }
            }
            drop(slot);
        })
        .map(drop)
        .map_err(drop)
}

/// A fetched picture for the render thread to decode.
pub struct Decode {
    owed: Owed,
    loaded: Loaded,
    term: Budget,
}

/// A `data:` picture for the render thread to decode, from its page's
/// tree.
pub struct Data {
    owed: Owed,
    tree: BudgetShared<Tree>,
    node: NodeId,
    term: Budget,
}

/// Whether `src` is a `data:` URL, as `gather` reads one, before it is
/// joined: a long one is past td-url's `MAX_URL`.
fn is_data(src: &str) -> bool {
    let src = src.trim_start_matches(|c: char| c.is_ascii() && (c as u8) <= b' ');
    src.get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
}

/// What a picture's body is, sniffed as browsers sniff an image (MIME
/// Sniffing, "Sniffing in an image context"): by its signature, whatever
/// its type says, unless that type is XML's, which SVG's is.
fn format(content_type: Option<&str>, body: &[u8]) -> Option<Format> {
    let essence = content_type
        .and_then(|kind| kind.split(';').next())
        .map(|kind| kind.trim_matches(|c: char| c.is_ascii_whitespace()))
        .unwrap_or("");
    let xml = essence.len() >= 4
        && essence
            .get(essence.len() - 4..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case("+xml"))
        || essence.eq_ignore_ascii_case("text/xml")
        || essence.eq_ignore_ascii_case("application/xml");
    if xml {
        return None;
    }
    Format::sniff(body)
}

/// `body` decoded under `term` into what picture `at` comes to.
fn decoded(body: &[u8], content_type: Option<&str>, term: &Budget, told: &Inbox) -> Outcome {
    let Some(format) = format(content_type, body) else {
        return Outcome::Refused(Why::Failed);
    };
    match picture::decode_inline(format, body, term) {
        Ok((picture, natural)) => {
            let inline = Inline::new(picture, natural, told.witness());
            match BudgetShared::new(term, inline) {
                Ok(inline) => Outcome::Shown(inline),
                Err(_) => Outcome::Refused(Why::Room),
            }
        }
        Err(why) if why.resources() => Outcome::Refused(Why::Room),
        Err(_) => Outcome::Refused(Why::Failed),
    }
}

/// On the render thread: a fetched picture decoded, its body let go,
/// and put in its page's inbox, or let go here when the page has gone.
pub(crate) fn decode(job: Decode) {
    let Decode { owed, loaded, term } = job;
    if owed.gone() {
        return;
    }
    let reply = &loaded.reply;
    let outcome = decoded(
        &reply.body,
        reply.header("content-type"),
        &term,
        &owed.inbox,
    );
    drop(loaded);
    owed.answer(outcome);
}

/// On the render thread: a `data:` picture decoded from its `src`, with
/// no request, its decoding's scratch and bytes in the pictures term.
pub(crate) fn decode_data(job: Data) {
    let Data {
        owed,
        tree,
        node,
        term,
    } = job;
    if owed.gone() {
        return;
    }
    let outcome = match tree
        .src(node)
        .and_then(|src| td_page::gather::data(src, &term))
    {
        Some(Ok(Some(bytes))) if bytes.len() as u64 <= MAX_PICTURE_BODY => {
            decoded(&bytes, None, &term, &owed.inbox)
        }
        Some(Ok(Some(_))) => Outcome::Refused(Why::Failed),
        Some(Err(_)) => Outcome::Refused(Why::Room),
        Some(Ok(None)) | None => Outcome::Refused(Why::Skipped),
    };
    drop(tree);
    owed.answer(outcome);
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
pub(crate) mod tests {
    use super::*;

    /// The pages watched, by URL: each one's pictures, decoded pictures
    /// and fitted copies tell where they are dropped.
    static WATCHED: Mutex<Vec<(String, Told)>> = Mutex::new(Vec::new());

    /// Watches the pictures of pages at `url` made from now on.
    pub(crate) fn watch(url: &str) -> std::sync::mpsc::Receiver<(&'static str, bool)> {
        let (tell, told) = std::sync::mpsc::channel();
        WATCHED.lock().unwrap().push((url.to_string(), tell));
        told
    }

    /// How many of `pictures` have come, laid or not.
    pub(crate) fn came(pictures: &Pictures) -> u32 {
        pictures.came
    }

    /// A handle on `pictures`' inbox, as a fetch thread holds one.
    pub(crate) fn inbox(pictures: &Pictures) -> BudgetShared<Inbox> {
        pictures.inbox.try_clone().unwrap()
    }

    /// How many answers wait in `pictures`' inbox, not yet taken.
    pub(crate) fn ready(pictures: &Pictures) -> usize {
        let places = pictures.inbox.places.lock().unwrap();
        places
            .iter()
            .filter(|place| matches!(place, Place::Ready(_)))
            .count()
    }

    /// How many of `pictures` are asked for and not yet answered.
    pub(crate) fn asked(pictures: &Pictures) -> usize {
        pictures
            .entries
            .iter()
            .filter(|entry| matches!(entry.state, State::Asked))
            .count()
    }

    /// Whether the pictures whose inbox this is have been let go.
    pub(crate) fn gone(inbox: &Inbox) -> bool {
        inbox.gone.load(Ordering::SeqCst)
    }

    /// The witness of the pictures of a page at `url`, if watched: told
    /// what of them dropped and whether on the render thread.
    pub(super) fn witness(url: &Url) -> Option<Witness> {
        named(url)
            .map(|told| -> Witness { Box::leak(Box::new(move |what| tell(Some(&told), what))) })
    }

    pub(super) fn named(url: &Url) -> Option<Told> {
        let text = url.text();
        let watched = WATCHED.lock().unwrap_or_else(PoisonError::into_inner);
        watched
            .iter()
            .find(|(watched, _)| *watched == text)
            .map(|(_, tell)| tell.clone())
    }

    fn png(width: usize, height: usize) -> Vec<u8> {
        td_png::encode_rgb8(width, height, &[200, 100, 50].repeat(width * height)).unwrap()
    }

    /// An inline picture's decoding is charged its whole work before it
    /// begins, refused one byte short of it, then holds exactly its
    /// pixels; a fitted copy holds exactly its own, refused one byte
    /// short.
    #[test]
    fn an_inline_picture_is_charged_its_work_then_its_pixels_exactly() {
        let body = png(7, 5);
        let plan = picture::plan(Format::Png, &body).unwrap();
        let work = picture::work(&plan, body.len());
        assert!(work > 7 * 5 * 4);
        let base = Budget::new(1 << 30).unwrap().used();
        let short = Budget::new(base + work - 1).unwrap();
        let refused = picture::decode_inline(Format::Png, &body, &short)
            .err()
            .unwrap();
        assert!(refused.resources());
        assert_eq!(short.used(), base);
        let term = Budget::new(base + work).unwrap();
        let (picture, natural) = picture::decode_inline(Format::Png, &body, &term).unwrap();
        assert_eq!(term.used(), base + 7 * 5 * 4);
        assert_eq!(natural, (7, 5));
        let inline = Inline::new(picture, natural, None);
        // Its pixels and its scratch, both held while it is made; its
        // steps from the layout's work.
        let cost = inline.picture().fit_exact_cost(9, 4).unwrap();
        assert!(cost.scratch > 0 && cost.steps > 0);
        let (idle, cancel) = (AtomicBool::new(false), AtomicBool::new(true));
        let fits = Budget::new(base + 9 * 4 * 4 + cost.scratch).unwrap();
        let mut work = cost.steps;
        let copy = inline.fit(9, 4, &fits, &mut work, &idle).unwrap();
        assert_eq!((fits.used(), work), (base + 9 * 4 * 4, 0));
        assert_eq!((copy.fitted().width, copy.fitted().height), (9, 4));
        drop(copy);
        assert_eq!(fits.used(), base);
        let mut work = cost.steps - 1;
        assert!(inline.fit(9, 4, &fits, &mut work, &idle).is_none());
        assert_eq!((fits.used(), work), (base, cost.steps - 1));
        let short = Budget::new(base + 9 * 4 * 4 + cost.scratch - 1).unwrap();
        let mut work = cost.steps;
        assert!(inline.fit(9, 4, &short, &mut work, &idle).is_none());
        // Refused for room: none of the layout's work spent.
        assert_eq!((short.used(), work), (base, cost.steps));
        let mut work = cost.steps;
        assert!(inline.fit(9, 4, &fits, &mut work, &cancel).is_none());
        assert_eq!(fits.used(), base);
        drop(inline);
        assert_eq!(term.used(), base);
    }

    /// What is not a picture, or not one td-web decodes, is refused as
    /// failed; one past the term's room as wanting room.
    #[test]
    fn a_body_decodes_to_what_it_is() {
        let inbox = Inbox {
            places: Mutex::new(std::array::from_fn(|_| Place::Empty)),
            gone: AtomicBool::new(false),
            told: None,
            witness: None,
        };
        let term = Budget::new(64 << 20).unwrap();
        let why = |outcome: Outcome| match outcome {
            Outcome::Shown(_) => None,
            Outcome::Refused(why) => Some(why),
        };
        let body = png(3, 3);
        assert_eq!(why(decoded(&body, Some("image/gif"), &term, &inbox)), None);
        assert_eq!(
            why(decoded(&body, Some("image/svg+xml"), &term, &inbox)),
            Some(Why::Failed)
        );
        assert_eq!(
            why(decoded(&body[..body.len() - 8], None, &term, &inbox)),
            Some(Why::Failed)
        );
        let small = Budget::new(1 << 20).unwrap();
        assert_eq!(why(decoded(&body, None, &small, &inbox)), Some(Why::Room));
    }

    /// Increment 9's host measurement (DESIGN.md §10, "Memory"): each
    /// picture file in `TD_WEB_CORPUS` decoded as an inline picture is,
    /// under a term with room: its size, the work charged for it and the
    /// resident peak's growth decoding it, the peak reset first. A
    /// process a picture: `TD_WEB_ONLY` names its file. Release build.
    #[test]
    #[ignore = "host measurement: TD_WEB_CORPUS, release build"]
    fn measure_decoding() {
        let dir = std::path::PathBuf::from(std::env::var_os("TD_WEB_CORPUS").unwrap());
        let only = std::env::var("TD_WEB_ONLY").unwrap();
        let kib = |name: &str| -> usize {
            let status = std::fs::read_to_string("/proc/self/status").unwrap();
            let line = status.lines().find(|l| l.starts_with(name)).unwrap();
            line.split_whitespace().nth(1).unwrap().parse().unwrap()
        };
        let body = std::fs::read(dir.join(&only)).unwrap();
        let format = format(None, &body).unwrap();
        let plan = picture::plan(format, &body).unwrap();
        let work = picture::work(&plan, body.len());
        let term = Budget::new(1 << 30).unwrap();
        std::fs::write("/proc/self/clear_refs", "5").unwrap();
        let before = kib("VmRSS:");
        let started = std::time::Instant::now();
        let decoded = picture::decode_inline(format, &body, &term);
        let took = started.elapsed();
        let peak = kib("VmHWM:").saturating_sub(before) * 1024;
        match decoded {
            Ok((picture, _)) => println!(
                "{only}\t{}x{}\tbody {}\twork {}\tpeak {}\tratio {:.2}\t{took:.1?}",
                picture.width,
                picture.height,
                body.len(),
                work,
                peak,
                peak as f64 / work as f64,
            ),
            Err(why) => println!("{only}\trefused {why}"),
        }
    }

    #[test]
    fn an_svg_is_known_by_its_path() {
        assert!(svg("https://example.org/a.svg"));
        assert!(svg("https://example.org/a.SVG?x=1"));
        assert!(svg("https://example.org/a.svgz#f"));
        assert!(!svg("https://example.org/a.png?f=.svg"));
        assert!(!svg("https://example.org/svg"));
    }

    #[test]
    fn a_picture_is_sniffed_whatever_its_type_but_xml() {
        let png = b"\x89PNG\r\n\x1a\nrest";
        assert_eq!(format(Some("image/jpeg"), png), Some(Format::Png));
        assert_eq!(format(Some("text/plain"), png), Some(Format::Png));
        assert_eq!(format(None, png), Some(Format::Png));
        assert_eq!(format(Some(" image/svg+xml "), png), None);
        assert_eq!(format(Some("Text/XML; charset=utf-8"), png), None);
        assert_eq!(format(Some("image/png"), b"<svg>"), None);
    }

    /// Each picture has its own place: an answer is never lost, and one
    /// put twice, or after the page has gone, is given back.
    #[test]
    fn each_picture_is_answered_once_and_none_after_its_page_has_gone() {
        let inbox = Inbox {
            places: Mutex::new(std::array::from_fn(|_| Place::Empty)),
            gone: AtomicBool::new(false),
            told: None,
            witness: None,
        };
        for at in 0..MAX_PICTURES {
            assert!(inbox.put(at, Outcome::Refused(Why::Failed)).is_none());
        }
        assert!(inbox.put(3, Outcome::Refused(Why::Room)).is_some());
        assert!(inbox
            .put(MAX_PICTURES, Outcome::Refused(Why::Room))
            .is_some());
        inbox.clear();
        assert!(inbox.gone.load(Ordering::SeqCst));
        let places = inbox.places.lock().unwrap();
        assert!(places.iter().all(|place| matches!(place, Place::Taken)));
        drop(places);
        assert!(inbox.put(0, Outcome::Refused(Why::Failed)).is_some());
    }

    /// A picture with a pixel not opaque is blended, and charged so; an
    /// opaque one is copied.
    #[test]
    fn a_picture_knows_whether_it_is_opaque() {
        let inbox = Inbox {
            places: Mutex::new(std::array::from_fn(|_| Place::Empty)),
            gone: AtomicBool::new(false),
            told: None,
            witness: None,
        };
        let term = Budget::new(64 << 20).unwrap();
        for (body, opaque) in [(png(3, 2), true), (picture::tests::GIF.to_vec(), false)] {
            let Outcome::Shown(inline) = decoded(&body, None, &term, &inbox) else {
                panic!("not shown");
            };
            assert_eq!(inline.opaque(), opaque);
            let work = &mut FIT_WORK.clone();
            let copy = inline
                .fit(5, 5, &term, work, &AtomicBool::new(false))
                .unwrap();
            assert_eq!(copy.opaque(), opaque);
        }
    }

    /// A picture come and never taken goes with its pictures, though a
    /// fetch thread still holds their inbox; and an answer owed is put
    /// failed as it drops, unless it was answered or handed back.
    #[test]
    fn what_the_inbox_holds_goes_with_the_pictures() {
        let laid = crate::made::laid("<img src=/a.png><img src=/b.png>");
        let url = Url::parse("https://inbox.example/").unwrap();
        let told = watch("https://inbox.example/");
        let page = Budget::new(64 << 20).unwrap();
        let mut pictures = Pictures::new(laid.tree(), &url, &page, 16 << 20, FIT_WORK)
            .unwrap()
            .unwrap();
        let held = inbox(&pictures);
        let outcome = decoded(&png(2, 2), None, &pictures.term, &held);
        assert!(matches!(outcome, Outcome::Shown(_)));
        assert!(held.put(0, outcome).is_none());
        // Owed and dropped: failed. Handed back: owed no more.
        drop(Owed {
            at: 1,
            inbox: inbox(&pictures),
            armed: true,
        });
        assert!(pictures.take());
        assert_eq!(pictures.refused(1), Some(Why::Failed));
        assert_eq!(pictures.refused(0), None);
        drop(pictures);
        let pictures = Pictures::new(laid.tree(), &url, &page, 16 << 20, FIT_WORK)
            .unwrap()
            .unwrap();
        let held = inbox(&pictures);
        let outcome = decoded(&png(2, 2), None, &pictures.term, &held);
        assert!(held.put(0, outcome).is_none());
        Owed {
            at: 1,
            inbox: inbox(&pictures),
            armed: true,
        }
        .disarm();
        drop(pictures);
        let drops: Vec<_> = told.try_iter().map(|(what, _)| what).collect();
        // Each `Pictures` tells as its drop begins, then lets its entries
        // go; the second's untaken picture goes as its inbox is cleared.
        assert_eq!(drops, ["pictures", "inline", "inline", "pictures"]);
        assert!(gone(&held));
        assert!(held.put(1, Outcome::Refused(Why::Failed)).is_some());
        drop(held);
    }
}
