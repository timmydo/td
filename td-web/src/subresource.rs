//! Subresources fetched (DESIGN.md §10, "Subresources" and "Slots"): at
//! most `MAX_SUBRESOURCES` fetches out across the window's pages, stopped
//! ones included until the service answers, each on a thread of its own
//! and each hop asked within its page's `STYLE_WAIT`; and a navigation's
//! page waiting for its sheets, on the navigation's fetch thread, until
//! they settle or `STYLE_WAIT` passes, the render thread decoding each as
//! it comes.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::document::{Document, Dropped, Made, Renderer, Styling};
use crate::fetch::{self, Ask, Failure, Loaded};
use td_alloc::{Budget, BudgetError, BudgetShared, BudgetString};
use td_page::gather::{self, Refused};
use td_url::Url;

/// The most subresource fetches out at once, across the window's pages.
pub const MAX_SUBRESOURCES: usize = 4;
/// The counted requests td-web's fetch service serves at once: its
/// `[web-fetch]` unit's `--workers` and td-launch's `WEB_FETCH_WORKERS` on a
/// host, which a recipe test holds equal to this.
pub const SERVICE_WORKERS: usize = 8;
// Every navigation and subresource out, stopped ones included, has a
// service worker of its own, so a navigation never waits behind a sheet.
const _: () = assert!(crate::app::MAX_WORKERS + MAX_SUBRESOURCES <= SERVICE_WORKERS);
/// How long after its reply a page waits for its sheets.
pub const STYLE_WAIT: Duration = Duration::from_secs(3);
/// How often a waiting page looks at its stop flag and for a free slot.
pub const TICK: Duration = Duration::from_millis(20);
/// What a sheet asks: `public` whatever its page's reach, CSS, at most
/// td-css's bound for one sheet; its page's deadline is set as it asks.
pub(crate) const SHEET: Ask = Ask {
    public: true,
    accept: gather::ACCEPT,
    limit: gather::MAX_SHEET as u64,
    deadline: None,
};

/// The window's subresource slots: how many fetches are out.
pub struct Slots(BudgetShared<AtomicUsize>);

/// One subresource fetch out, held until its body is let go.
pub struct Slot(BudgetShared<AtomicUsize>);

impl Slots {
    pub fn new(memory: &Budget) -> Result<Self, BudgetError> {
        BudgetShared::new(memory, AtomicUsize::new(0)).map(Self)
    }

    pub fn try_clone(&self) -> Result<Self, BudgetError> {
        self.0.try_clone().map(Self)
    }

    /// A slot, when fewer than `MAX_SUBRESOURCES` are out.
    fn take(&self) -> Option<Slot> {
        self.take_within(MAX_SUBRESOURCES)
    }

    /// A slot, when fewer than `most` are out: pictures leave room for
    /// sheets (DESIGN.md §10, "Slots").
    pub(crate) fn take_within(&self, most: usize) -> Option<Slot> {
        let most = most.min(MAX_SUBRESOURCES);
        let out = self.0.try_clone().ok()?;
        let mut held = self.0.load(Ordering::SeqCst);
        loop {
            if held >= most {
                return None;
            }
            match self
                .0
                .compare_exchange(held, held + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Some(Slot(out)),
                Err(now) => held = now,
            }
        }
    }

    /// How many fetches are out, sheets' and pictures' together.
    pub fn out(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A sheet that came, its slot held until the render thread lets its
/// body go.
pub struct Arrived {
    pub at: usize,
    pub loaded: Loaded,
    _slot: Slot,
}

/// The sheets a page hands the render thread at once: one a slot.
pub type Arrivals = [Option<Arrived>; MAX_SUBRESOURCES];

/// What a sheet's fetch thread answers, each holding its slot until it is
/// taken, so no page has more answers waiting than there are slots.
enum Answer {
    Came(Arrived),
    Refused(usize, Refused, Slot),
}

/// Where a page's sheet fetches answer: a place for each slot, since each
/// answer holds one until it is taken; and whether the page has gone on
/// without them, stopped, styled or late, which each reads before each
/// hop.
struct Mailbox {
    answers: Mutex<[Option<Answer>; MAX_SUBRESOURCES]>,
    ready: Condvar,
    gone: AtomicBool,
}

/// Sets `gone` however the page's wait ends, and lets go of the answers
/// left, so their slots free at once.
struct Leaving<'a>(&'a Mailbox);

impl Drop for Leaving<'_> {
    fn drop(&mut self) {
        self.0.gone.store(true, Ordering::SeqCst);
        // Taken under the lock `put` reads `gone` under, so none is put
        // after; dropped once it is released.
        let left = std::mem::take(
            &mut *self
                .0
                .answers
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        drop(left);
    }
}

impl Mailbox {
    /// `answer` left for the page, or, the page gone, dropped here with
    /// its body and slot. A place is always free: each answer waiting
    /// holds one of the `MAX_SUBRESOURCES` slots.
    fn put(&self, answer: Answer) {
        let mut answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        let unkept = if self.gone.load(Ordering::SeqCst) {
            Some(answer)
        } else {
            match answers.iter_mut().find(|answer| answer.is_none()) {
                Some(free) => {
                    *free = Some(answer);
                    None
                }
                None => Some(answer),
            }
        };
        drop(answers);
        drop(unkept);
        self.ready.notify_one();
    }

    /// The answers come, each taken by `each`, after waiting at most
    /// `wait` for the first when none has.
    fn take(&self, wait: Duration, mut each: impl FnMut(Answer)) {
        let mut answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        if !wait.is_zero() && answers.iter().all(Option::is_none) {
            answers = match self.ready.wait_timeout(answers, wait) {
                Ok((answers, _)) => answers,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        for answer in answers.iter_mut() {
            if let Some(answer) = answer.take() {
                each(answer);
            }
        }
    }
}

/// Whether a sheet that answered is applied: a status `2xx` and a type
/// `text/css`, or none given, as Fetch takes a style sheet's (DESIGN.md
/// §10, "Requests").
pub(crate) fn acceptable(loaded: &Loaded) -> bool {
    let css = loaded.reply.header("content-type").is_none_or(|kind| {
        let essence = kind.split(';').next().unwrap_or("");
        essence.trim().eq_ignore_ascii_case("text/css")
    });
    (200..300).contains(&loaded.reply.status) && css
}

/// What a sheet's fetch thread is given: where it is fetched from, its
/// page's reach and deadline, and where it answers.
struct Asked {
    at: usize,
    url: BudgetString,
    https: bool,
    deadline: Instant,
    mailbox: BudgetShared<Mailbox>,
}

/// Fetches sheet `asked.at` on a thread of its own, holding `slot` until
/// its answer is taken, and a body that came until it is let go. A page
/// stopped, or past its deadline, before a hop asks no more and answers
/// nothing: it is let go, or late.
fn spawn(fetcher: &fetch::Shared, asked: Asked, slot: Slot) -> Result<(), ()> {
    let fetcher = fetch::Shared::clone(fetcher);
    let builder = match td_alloc::string("td-web sheet") {
        Ok(name) => std::thread::Builder::new().name(name),
        Err(_) => std::thread::Builder::new(),
    };
    builder
        .spawn(move || {
            let Asked {
                at,
                url,
                https,
                deadline,
                mailbox,
            } = asked;
            let ask = Ask {
                deadline: Some(deadline),
                ..SHEET
            };
            let fetched = match Url::parse(url.as_str()) {
                Ok(url) => fetch::subresource(fetcher.as_ref(), url, https, ask, &mailbox.gone),
                Err(_) => Err(Failure::Fetch),
            };
            drop(url);
            let answer = match fetched {
                Ok(loaded) if acceptable(&loaded) => Answer::Came(Arrived {
                    at,
                    loaded,
                    _slot: slot,
                }),
                Ok(_) | Err(Failure::Fetch | Failure::Mixed | Failure::Redirect) => {
                    Answer::Refused(at, Refused::Failed, slot)
                }
                Err(Failure::Stopped) => return,
            };
            // A page no longer waiting lets the body go with the last of
            // the mailbox, here, a fetch thread's own allocation.
            mailbox.put(answer);
        })
        .map(drop)
        .map_err(drop)
}

/// The page `styling` once its sheets have settled, or once `deadline`
/// passes with those that came by then; none once `stopped`, the page
/// let go on the render thread. Its sheets are fetched as slots free
/// until the deadline, each handed to the render thread as it comes so
/// its imports are asked for at once.
pub fn gather(
    fetcher: &fetch::Shared,
    renderer: &Renderer,
    slots: &Slots,
    styling: Styling,
    deadline: Instant,
    stopped: &AtomicBool,
) -> Option<Document> {
    let mailbox = Mailbox {
        answers: Mutex::new(Default::default()),
        ready: Condvar::new(),
        gone: AtomicBool::new(false),
    };
    let Ok(mailbox) = BudgetShared::new(renderer.budget(), mailbox) else {
        // No room to wait: styled now with what it has.
        return finished(
            renderer,
            renderer.styles(styling, Default::default(), true)?,
        );
    };
    let _leaving = Leaving(&mailbox);
    let mut styling = styling;
    let mut held: Arrivals = Default::default();
    loop {
        if stopped.load(Ordering::SeqCst) {
            // Its fetches told first, so none asks another hop once the
            // page is gone.
            mailbox.gone.store(true, Ordering::SeqCst);
            drop(held);
            renderer.let_go(Dropped::Styling(styling));
            return None;
        }
        let finish = Instant::now() >= deadline;
        // Ask for what is wanted, in order, while slots last and there
        // is time.
        while let Some((at, url)) = styling.wanted().filter(|_| !finish) {
            let Some(slot) = slots.take() else {
                break;
            };
            let copied = BudgetString::copy(renderer.budget(), url);
            let Ok((url, mailbox)) = copied.and_then(|url| Ok((url, mailbox.try_clone()?))) else {
                styling.refused(at, Refused::Skipped);
                continue;
            };
            let asked = Asked {
                at,
                url,
                https: styling.https(),
                deadline,
                mailbox,
            };
            match spawn(fetcher, asked, slot) {
                Ok(()) => styling.asked(at),
                Err(()) => styling.refused(at, Refused::Failed),
            }
        }
        // What has come, each holding a slot, so no more than there are.
        mailbox.take(Duration::ZERO, |answer| {
            take(&mut styling, &mut held, answer)
        });
        if finish || held.iter().any(Option::is_some) || !styling.unsettled() {
            let arrived = std::mem::take(&mut held);
            match renderer.styles(styling, arrived, finish)? {
                Made::Document(document) => return Some(document),
                Made::Styles(next) => {
                    styling = next;
                    continue;
                }
            }
        }
        let wait = deadline.saturating_duration_since(Instant::now()).min(TICK);
        mailbox.take(wait, |answer| take(&mut styling, &mut held, answer));
    }
}

/// A finished styling's document; a page asked to wait again is not
/// reached, since `finish` styles it, and is let go on the render
/// thread.
fn finished(renderer: &Renderer, made: Made) -> Option<Document> {
    match made {
        Made::Document(document) => Some(document),
        Made::Styles(styling) => {
            renderer.let_go(Dropped::Styling(styling));
            None
        }
    }
}

/// An answer taken: a sheet that came held for the render thread, one
/// refused counted, its slot given back.
fn take(styling: &mut Styling, held: &mut Arrivals, answer: Answer) {
    match answer {
        Answer::Came(arrived) => match held.iter_mut().find(|slot| slot.is_none()) {
            Some(free) => *free = Some(arrived),
            // Not reached: each held holds one of the slots.
            None => styling.refused(arrived.at, Refused::Skipped),
        },
        Answer::Refused(at, why, slot) => {
            drop(slot);
            styling.refused(at, why);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::unreachable)]
mod tests {
    use super::*;
    use crate::fetch::Reply;
    use crate::made::{markup, measure, WANT};
    use std::sync::atomic::AtomicBool;
    use td_alloc::Budget;

    fn loaded(status: u16, kind: Option<&str>) -> Loaded {
        Loaded {
            url: Url::parse("https://example.org/a.css").unwrap(),
            fragment: None,
            reply: Reply {
                status,
                headers: kind
                    .map(|kind| ("content-type".to_string(), kind.to_string()))
                    .into_iter()
                    .collect(),
                body: Vec::new(),
            },
        }
    }

    fn mailbox() -> Mailbox {
        Mailbox {
            answers: Mutex::new(Default::default()),
            ready: Condvar::new(),
            gone: AtomicBool::new(false),
        }
    }

    /// Every answer holds its slot until taken, so the slots out bound
    /// the answers waiting, which the mailbox has a place for each of.
    #[test]
    fn no_answer_is_lost_however_fast_refusals_come() {
        let memory = Budget::new(1 << 20).unwrap();
        let slots = Slots::new(&memory).unwrap();
        let mailbox = mailbox();
        for at in 0..MAX_SUBRESOURCES {
            let slot = slots.take().unwrap();
            mailbox.put(Answer::Refused(at, Refused::Failed, slot));
        }
        // Nothing more is asked until one is taken.
        assert!(slots.take().is_none());
        let mut taken = Vec::new();
        mailbox.take(Duration::ZERO, |answer| match answer {
            Answer::Refused(at, _, _) => taken.push(at),
            Answer::Came(_) => unreachable!(),
        });
        assert_eq!(taken, (0..MAX_SUBRESOURCES).collect::<Vec<_>>());
        assert_eq!(slots.out(), 0);
    }

    /// A page gone: an answer is dropped as it comes and those left are
    /// let go as it leaves, so their slots free without waiting for
    /// anything else.
    #[test]
    fn a_page_gone_frees_its_answers_slots_at_once() {
        let memory = Budget::new(1 << 20).unwrap();
        let slots = Slots::new(&memory).unwrap();
        let mailbox = mailbox();
        let leaving = Leaving(&mailbox);
        mailbox.put(Answer::Refused(0, Refused::Failed, slots.take().unwrap()));
        assert_eq!(slots.out(), 1);
        drop(leaving);
        assert!(mailbox.gone.load(Ordering::SeqCst));
        assert_eq!(slots.out(), 0);
        mailbox.put(Answer::Refused(1, Refused::Failed, slots.take().unwrap()));
        assert_eq!(slots.out(), 0);
    }

    #[test]
    fn a_sheet_is_css_by_its_types_essence_or_with_no_type() {
        assert!(acceptable(&loaded(200, Some("text/css"))));
        assert!(acceptable(&loaded(206, Some(" Text/CSS ; charset=utf-8"))));
        // Fetch takes a style sheet with no type at all.
        assert!(acceptable(&loaded(200, None)));
        assert!(!acceptable(&loaded(200, Some("text/plain"))));
        assert!(!acceptable(&loaded(200, Some("text/css2"))));
        assert!(!acceptable(&loaded(404, Some("text/css"))));
        assert!(!acceptable(&loaded(304, None)));
    }

    /// Resident and peak resident KiB, and the peak reset to now.
    fn resident() -> (usize, usize) {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let kib = |name: &str| -> usize {
            let line = status.lines().find(|l| l.starts_with(name)).unwrap();
            line.split_whitespace().nth(1).unwrap().parse().unwrap()
        };
        (kib("VmRSS:"), kib("VmHWM:"))
    }

    fn reset_peak() {
        std::fs::write("/proc/self/clear_refs", "5").unwrap();
    }

    /// Increment 8's host measurement (DESIGN.md §10): each page of a
    /// captured corpus (`fetch::tests::Corpus`) parsed, its sheets
    /// gathered from the capture and compiled: the sheets linked and
    /// imported, the text held, the least sheets term that compiles them
    /// (by halving), and the resident peak's growth parsing and compiling
    /// past the bytes td-alloc holds after the step, `WORKING`'s part, the
    /// peak reset before each. A process a page: `TD_WEB_ONLY`.
    #[test]
    #[ignore = "host measurement: TD_WEB_CORPUS with a pages.tsv, release build"]
    fn measure_sheets() {
        use crate::fetch::{self, tests::Corpus, Fetcher};
        use td_page::gather::{Refused, Sheets};
        use td_page::pipeline::{parse, Parsed};
        use td_page::{memory, sheets};
        let dir = std::path::PathBuf::from(std::env::var_os("TD_WEB_CORPUS").unwrap());
        let corpus = Corpus::read(&dir).unwrap();
        let only = std::env::var("TD_WEB_ONLY").ok();
        let cancel = AtomicBool::new(false);
        for (name, address) in &corpus.pages {
            if only.as_ref().is_some_and(|only| only != name) {
                continue;
            }
            let reply = corpus
                .table
                .get(address, fetch::Ask::navigation(true))
                .unwrap();
            let text = String::from_utf8_lossy(&reply.body);
            let page = Budget::new(memory::PAGES).unwrap();
            let markup = markup(&page, &text);
            let held = page.used();
            reset_peak();
            let (before, _) = resident();
            let parsed = parse(markup, WANT, &mut measure(WANT), &cancel, &page).unwrap();
            let (_, peak) = resident();
            let tree_bytes = page.used() - held;
            let parse_over = (peak - before) as f64 / 1024.0 - tree_bytes as f64 / 1048576.0;
            let Parsed::Tree { tree, .. } = parsed else {
                println!("{name}: not a tree");
                continue;
            };
            let url = td_url::Url::parse(address).unwrap();
            let mut sheets = Sheets::new(&tree, &url, td_charset::Encoding::Utf8, &page).unwrap();
            while let Some((at, wanted)) = sheets.wanted() {
                let wanted = td_url::Url::parse(wanted).unwrap();
                sheets.asked(at);
                match fetch::subresource(&corpus, wanted, url.https, SHEET, &cancel) {
                    Ok(loaded) if acceptable(&loaded) => {
                        let kind = loaded.reply.header("content-type");
                        sheets
                            .arrived(at, &loaded.url, kind, &loaded.reply.body)
                            .unwrap();
                    }
                    _ => sheets.refused(at, Refused::Failed),
                }
            }
            sheets.finish();
            let environment = sheets::environment(WANT.width, WANT.height, u32::from(WANT.scale));
            let compile = |limit: usize| {
                let terms = page.child(limit + Budget::metadata_bytes()).unwrap();
                let compiled = sheets::compile(&tree, Some(&sheets), true, &environment, &terms);
                compiled.map(|(set, omitted)| (set, omitted, terms))
            };
            let room = memory::SHEETS - sheets.bytes();
            reset_peak();
            let (before, _) = resident();
            let started = std::time::Instant::now();
            let (set, omitted, terms) = compile(room).unwrap();
            let took = started.elapsed();
            let (_, peak) = resident();
            let set_bytes = terms.used();
            let compile_over = (peak - before) as f64 / 1024.0 - set_bytes as f64 / 1048576.0;
            drop(set);
            drop(terms);
            let (mut low, mut high) = (0, room);
            while high - low > 16 << 10 {
                let mid = low + (high - low) / 2;
                if compile(mid).is_ok() {
                    high = mid;
                } else {
                    low = mid;
                }
            }
            println!(
                "{name}: {} sheets, {:?}, {omitted:?}; tree {:.1} MiB, parsing {:.1} MiB \
                 past it; sheets {:.2} MiB held, set {:.1} MiB, compiled in {took:.1?} \
                 with {:.1} MiB past it, the least term compiling them {:.1} MiB",
                sheets.taken(),
                sheets.tally(),
                tree_bytes as f64 / 1048576.0,
                parse_over,
                sheets.bytes() as f64 / 1048576.0,
                set_bytes as f64 / 1048576.0,
                compile_over,
                (high + sheets.bytes()) as f64 / 1048576.0,
            );
        }
    }
}
