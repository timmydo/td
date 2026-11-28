//! A message's HTML as a styled page (README.md, "HTML mail"): made by
//! td-page from the HTML part, its `style` elements and `style`
//! attributes, and the pictures its `cid:` URLs name among the message's
//! own parts (`pictures`), never a sheet or picture it names elsewhere;
//! and above it td-mail's own text of the headers and the
//! attachments, a page of its own the message's styles cannot reach,
//! under a rule td-mail draws. The two scroll as one document. Both are
//! made on td-mail's page thread (`ui::worker`) within one budget, made
//! again when the body's size changes, laid out again there for the
//! pictures that come while shown, and let go, or their making
//! cancelled, by the view before it renders a text.

use std::cell::RefCell;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::ui::pictures::{self, Come, Pictures};
use crate::ui::worker::{self, Job, Pending, Taken};
use td_alloc::{Budget, BudgetShared, BudgetString, BudgetVec};
use td_page::fonts::Measure;
use td_page::inline::Set;
use td_page::page::{Laid, Want};
use td_page::paint::{self, Frame, Marks, Scratch};
use td_page::pipeline::{self, Again, Page, Refusal};
use td_page::tree::Tree;
use td_page::view::{Pane, View};
use td_ui::prose::{Faces, Metrics};
use td_ui::raster::{Draw, Primitive, Rect, Scale, TextFamily, BORDER, PAPER};

/// The pictures come that `tree`'s `img`s name by their `cid:` URLs, by
/// node, charged to the page's `budget`, and how many: none when none is
/// named or td-alloc refuses.
fn set_of(tree: &Tree, budget: &Budget, pictures: &Come) -> Option<(Set, u32)> {
    let mut shown = BudgetVec::new(budget).ok()?;
    for &node in tree.pictures() {
        let Some(inline) = tree.src(node).and_then(|src| pictures.named(src)) else {
            continue;
        };
        shown
            .try_push((node.index(), inline.try_clone().ok()?))
            .ok()?;
    }
    let laid = u32::try_from(shown.as_slice().len()).ok()?;
    (laid > 0).then_some(())?;
    Some((
        Set::new(shown, pictures.fitted(), pictures::FIT_WORK).ok()?,
        laid,
    ))
}

/// `page` laid again with the pictures come for its `img`s' `cid:` URLs;
/// as it was when none is named. Refused with them, it is laid again
/// without them at its rung; refused so, it steps down td-page's ladder.
fn with_pictures(
    page: Page,
    pictures: Option<&Come>,
    want: Want,
    measure: &mut Measure,
    cancel: &AtomicBool,
) -> Result<Page, Refusal> {
    let Some(pictures) = pictures.filter(|pictures| pictures.count() > 0) else {
        return Ok(page);
    };
    let Some((set, laid)) = set_of(&page.tree, &page.budget, pictures) else {
        return Ok(page);
    };
    let Page {
        tree,
        markup,
        sheets,
        laid: old,
        rung,
        notes,
        budget,
        ..
    } = page;
    drop(old);
    // Refused with pictures, `again` keeping nothing returns the refusal
    // rather than stepping down: the page is laid again without them at
    // its rung, as it was made, and only refused so steps down.
    let parts = |pictures: Option<Set>, want: Want, keep: bool| -> Result<Again, Refusal> {
        Ok(Again {
            tree: tree.try_clone()?,
            markup: markup.as_ref().map(BudgetShared::try_clone).transpose()?,
            sheets: sheets.as_ref().map(BudgetShared::try_clone).transpose()?,
            rung,
            notes,
            budget: budget.try_clone()?,
            old: None,
            keep,
            want,
            pictures,
        })
    };
    let with = Want {
        pictures: laid,
        ..want
    };
    match pipeline::again(parts(Some(set), with, true)?, measure, cancel) {
        Ok(page) => Ok(page),
        Err(Refusal::Cancelled) => Err(Refusal::Cancelled),
        Err(Refusal::Limit(_)) => pipeline::again(parts(None, want, false)?, measure, cancel),
    }
}

/// A message's pages to make on the page thread (`Paged::build`).
pub struct MakeJob {
    key: String,
    head: String,
    /// The HTML as far as td-mail reads it, and whether it was cut there.
    markup: String,
    cut: bool,
    rect: Rect,
    scale: Scale,
    faces: Option<Faces>,
    limit: usize,
    pictures: Option<Come>,
}

impl MakeJob {
    pub fn run(self, cancel: &AtomicBool) -> Option<Paged> {
        Paged::build(
            &self.key,
            &self.head,
            &self.markup,
            self.cut,
            self.rect,
            self.scale,
            self.faces.as_ref(),
            self.limit,
            self.pictures.as_ref(),
            cancel,
        )
    }
}

/// A shown page to lay out again with the pictures come since, on the
/// page thread, its old layout kept beside the new within the page's
/// budget while the window paints it; refused so, the refusal.
pub struct LayJob {
    again: Again,
    pictures: Come,
    faces: Option<Faces>,
    scale: Scale,
}

impl LayJob {
    pub fn run(self, cancel: &AtomicBool) -> Result<Page, Refusal> {
        let metrics = Metrics::new(self.faces.as_ref(), self.scale);
        let mut measure = Measure::new(metrics, TextFamily::Sans);
        let mut again = self.again;
        let (set, laid) =
            set_of(&again.tree, &again.budget, &self.pictures).ok_or(Refusal::Limit("memory"))?;
        again.pictures = Some(set);
        again.want = Want {
            pictures: laid,
            ..again.want
        };
        pipeline::again(again, &mut measure, cancel)
    }
}

/// What a message's pages are made in: the HTML's tree, styles and
/// layouts, and the head's. A page past it steps down td-page's ladder,
/// to the user-agent sheets, then to its text.
pub const BUDGET: usize = 32 << 20;
/// The inset each side of the pages, in device pixels at scale one.
const INSET: i64 = 8;
/// The space above and below the rule between the head and the page,
/// and the rule's own height, in device pixels at scale one.
const GAP: i64 = 6;
const RULE: i64 = 1;

/// The bases a link is resolved against when the message names none:
/// one name under `.invalid`, which no host has, by both schemes. A link
/// that resolves to a different scheme against each took its scheme from
/// the base, so was relative, `//host` too, and is refused.
const NO_BASE_HTTPS: &str = "https://relative.invalid/";
const NO_BASE_HTTP: &str = "http://relative.invalid/";

/// The most of a link's host the status row shows; a longer one keeps
/// only its end, where the name it is registered under is, after an
/// ellipsis: its start, which the sender chooses, could name anyone.
const HOST_SHOWN: usize = 32;

/// `target`, an absolute URL in printable ASCII, as the status row shows
/// it: its host, port included, cut to its end past `HOST_SHOWN`, so a
/// long host cannot push its own end off the row.
fn shown_target(target: &str) -> Option<String> {
    let after = target.find("://")? + 3;
    let rest = target.get(after..)?;
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let host = rest.get(..end)?;
    if host.len() <= HOST_SHOWN {
        return td_alloc::format(format_args!("{target}")).ok();
    }
    let tail = host.len() - (HOST_SHOWN - 1);
    td_alloc::format(format_args!(
        "{}\u{2026}{}{}",
        target.get(..after)?,
        host.get(tail..)?,
        rest.get(end..)?,
    ))
    .ok()
}

/// What a link resolved to: an `http` or `https` URL and its fragment, or
/// why it is not opened.
fn fetched(reference: td_url::Reference) -> Result<(td_url::Url, Option<String>), String> {
    match reference {
        td_url::Reference::Fetch { url, fragment } => Ok((url, fragment)),
        td_url::Reference::Other { scheme } => {
            Err(td_alloc::format(format_args!("a {scheme} link")).unwrap_or_default())
        }
    }
}

/// A line's top and bottom below a page's top, in device pixels.
fn line_span(laid: &Laid, line: u32, scale: i64) -> Option<(i64, i64)> {
    let line = laid.lines().get(usize::try_from(line).ok()?)?;
    let s = scale as f64;
    Some((
        (line.y * s).floor() as i64,
        ((line.y + line.height) * s).ceil() as i64,
    ))
}

/// How far a laid page reaches below its top, in device pixels.
fn bottom(laid: &Laid, scale: i64) -> i64 {
    let o = laid.layout.overflow;
    ((o.y + o.height).max(laid.layout.height).max(0.0) * scale as f64).ceil() as i64
}

/// The band of `rect` from `top` to `end`, none when they leave nothing.
fn band(rect: Rect, top: i64, end: i64) -> Option<Rect> {
    let top = top.max(rect.y);
    let end = end.min(rect.y + i64::from(rect.height));
    let height = u32::try_from(end - top).ok().filter(|h| *h > 0)?;
    Some(Rect {
        y: top,
        height,
        ..rect
    })
}

/// A message shown as its pages: the head, a rule, then the page, laid
/// out for the body they fill and scrolled through it together.
pub struct Paged {
    key: BudgetString,
    /// The body laid out for, its scale and its line.
    rect: Rect,
    scale: i64,
    line: i64,
    head: Page,
    page: Page,
    /// The head's height, and the page's top, below the body's top in
    /// device pixels when scrolled to the top.
    head_height: i64,
    page_top: i64,
    /// Scrolled down, in device pixels.
    y: i64,
    /// The page's focused link and the one under the pointer, by their
    /// index among its links.
    focus: Option<usize>,
    hover: Option<usize>,
    /// The pointer as last seen, and whether a key has stepped the focus
    /// since: until the pointer moves, the link under it is not the one
    /// shown, so the status row and Return speak of the focused one.
    pointer: Option<(i64, i64)>,
    stepped: bool,
    /// A press named the link under it: its button held, td-ui reports no
    /// pointer, and the link stays named until the pointer is seen again.
    named: bool,
    /// The HTML was longer than td-mail's input limit and cut at it.
    cut: bool,
    /// How many of the message's pictures had come when it was made: one
    /// more come, it is made again.
    pictures: usize,
    /// The limit the message's HTML was refused by at every rung, when it
    /// was: td-mail's notice is shown in its place.
    pub refused: Option<&'static str>,
    measure: RefCell<Measure>,
    scratch: RefCell<Scratch>,
    /// The pages' budget, their parent: let go after them.
    budget: Budget,
    /// The thread they were made on.
    #[cfg(test)]
    made_on: Option<String>,
}

fn want(rect: Rect, scale: i64) -> Option<Want> {
    let css = |px: i64| (px.max(1) as f64) / scale as f64;
    Some(Want {
        width: css(i64::from(rect.width) - 2 * INSET * scale),
        height: css(i64::from(rect.height)),
        scale: u8::try_from(scale).ok()?,
        body: TextFamily::Sans,
        styles: true,
        pictures: 0,
    })
}

impl Paged {
    /// `build`'s pages for `html`, as far as td-mail reads it, on this
    /// thread: the tests'.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn make(
        key: &str,
        head: &str,
        html: &str,
        rect: Rect,
        scale: Scale,
        faces: Option<&Faces>,
        limit: usize,
        pictures: Option<&Pictures>,
    ) -> Option<Self> {
        let markup = crate::compose::HTML_LIMITS.read(html);
        let come = pictures.and_then(Pictures::come);
        let cancel = AtomicBool::new(false);
        Self::build(
            key,
            head,
            markup,
            markup.len() < html.len(),
            rect,
            scale,
            faces,
            limit,
            come.as_ref(),
            &cancel,
        )
    }

    /// The message whose view names it `key`: `head` as td-mail's text,
    /// `markup` as its page (`cut` at td-mail's input limit), laid out for
    /// `rect` at `scale` over `faces` within a budget of `limit` bytes,
    /// with the `pictures` come. None when td-alloc refuses even the
    /// head's, or when `cancel` is raised.
    #[allow(clippy::too_many_arguments)]
    fn build(
        key: &str,
        head: &str,
        markup: &str,
        cut: bool,
        rect: Rect,
        scale: Scale,
        faces: Option<&Faces>,
        limit: usize,
        pictures: Option<&Come>,
        cancel: &AtomicBool,
    ) -> Option<Self> {
        let s = i64::try_from(scale.value()).ok()?.max(1);
        let want = want(rect, s)?;
        let budget = Budget::new(limit).ok()?;
        let metrics = Metrics::new(faces, scale);
        let line = i64::from(metrics.line().height.max(1));
        let mut measure = Measure::new(metrics, TextFamily::Sans);
        let head_page = pipeline::text_page(head, want, &mut measure, cancel, &budget).ok()?;
        let head_height = head_page.laid.as_deref().map_or(0, |laid| bottom(laid, s));
        let page = BudgetString::copy(&budget, markup)
            .ok()
            .and_then(|markup| BudgetShared::new(&budget, markup).ok());
        let made = match page {
            Some(markup) => pipeline::markup_page(markup, want, &mut measure, cancel, &budget)
                .and_then(|page| with_pictures(page, pictures, want, &mut measure, cancel)),
            None => Err(Refusal::Limit("memory")),
        };
        let (page, refused) = match made {
            Ok(made) => (made, None),
            Err(Refusal::Limit(name)) => {
                let notice = "This message's HTML is past what td-mail lays out; \
                              h shows it as text.";
                let own = pipeline::own_page(notice, want, &mut measure, cancel, &budget).ok()?;
                (own, Some(name))
            }
            Err(Refusal::Cancelled) => return None,
        };
        Some(Self {
            key: BudgetString::copy(&budget, key).ok()?,
            rect,
            scale: s,
            line,
            head: head_page,
            page,
            head_height,
            page_top: head_height + (2 * GAP + RULE) * s,
            y: 0,
            focus: None,
            hover: None,
            pointer: None,
            stepped: false,
            named: false,
            cut,
            pictures: pictures.map_or(0, Come::count),
            refused,
            measure: RefCell::new(measure),
            scratch: RefCell::new(Scratch::new_empty()),
            budget,
            #[cfg(test)]
            made_on: std::thread::current().name().map(str::to_string),
        })
    }

    /// Whether it is the message `key` laid out for `rect` at `scale`.
    pub fn is(&self, key: &str, rect: Rect, scale: Scale) -> bool {
        self.key.as_str() == key
            && self.rect == rect
            && i64::try_from(scale.value()).is_ok_and(|s| s.max(1) == self.scale)
    }

    /// The body it was laid out for.
    #[cfg(test)]
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// What the status row says of how the page was made, when it was not
    /// made whole: its styles left out, laid out as its text, refused, or
    /// its HTML cut at the input limit.
    pub fn note(&self) -> Option<String> {
        let notes = self.page.notes;
        let rung = if let Some(limit) = self.refused {
            Some(("HTML past td-mail's limits", limit))
        } else if let Some(limit) = notes.text {
            Some(("HTML shown as its text", limit))
        } else {
            notes.styles.map(|limit| ("HTML without its styles", limit))
        };
        let cut = crate::compose::HTML_LIMITS.input >> 10;
        match (rung, self.cut) {
            (None, false) => None,
            (None, true) => {
                td_alloc::format(format_args!("HTML past {cut} KiB not shown; h: text")).ok()
            }
            (Some((what, limit)), false) => {
                td_alloc::format(format_args!("{what} ({limit}); h: text")).ok()
            }
            (Some((what, limit)), true) => td_alloc::format(format_args!(
                "{what} ({limit}), past {cut} KiB not shown; h: text"
            ))
            .ok(),
        }
    }

    /// The lines the body shows, which the page keys move by.
    pub fn rows(&self) -> usize {
        usize::try_from(i64::from(self.rect.height) / self.line)
            .unwrap_or(1)
            .max(1)
    }

    /// How far it scrolls: the head and the page, less the body.
    fn most(&self) -> i64 {
        let page = self
            .page
            .laid
            .as_deref()
            .map_or(0, |laid| bottom(laid, self.scale));
        (self.page_top + page - i64::from(self.rect.height)).max(0)
    }

    /// Scrolled by `lines` body lines; whether it moved.
    pub fn scroll_lines(&mut self, lines: isize) -> bool {
        self.scroll_by(self.line.saturating_mul(lines as i64))
    }

    /// Scrolled by `pages` bodies, less a line so one stays in view.
    pub fn scroll_pages(&mut self, pages: isize) -> bool {
        let step = (i64::from(self.rect.height) - self.line).max(self.line);
        self.scroll_by(step.saturating_mul(pages as i64))
    }

    /// To the top, or the bottom.
    pub fn scroll_to(&mut self, bottom: bool) -> bool {
        let was = self.y;
        self.y = if bottom { self.most() } else { 0 };
        self.y != was
    }

    fn scroll_by(&mut self, dy: i64) -> bool {
        let was = self.y;
        self.y = self.y.saturating_add(dy).clamp(0, self.most());
        self.y != was
    }

    /// The page's top in the window, as scrolled.
    fn page_y(&self) -> i64 {
        self.rect.y - self.y + self.page_top
    }

    /// The link under a window point, by its index among the page's
    /// links: on the page below the rule alone.
    pub fn link_at(&self, x: i64, y: i64) -> Option<usize> {
        band(self.rect, self.page_y(), i64::MAX).filter(|page| page.contains(x, y))?;
        let laid = self.page.laid.as_deref()?;
        // td-page's view of the page alone, scrolled so its frame is the
        // one `emit` paints it in.
        let view = View {
            y: self.y - self.page_top,
            ..View::default()
        };
        let pane = Pane {
            rect: self.rect,
            inset: INSET * self.scale,
            scale: self.scale,
            line: self.line,
        };
        let mut measure = self.measure.try_borrow_mut().ok()?;
        view.link_at(laid, &mut measure, &pane, x, y)
    }

    /// The link under `at`, a window point or none; whether that changed.
    /// After a key step it is none until the pointer moves; after a press
    /// named a link, an absent pointer leaves it named.
    pub fn hover(&mut self, at: Option<(i64, i64)>) -> bool {
        if at.is_none() && self.named {
            return false;
        }
        self.named = false;
        if at != self.pointer {
            self.stepped = false;
        }
        self.pointer = at;
        let hover = at
            .filter(|_| !self.stepped)
            .and_then(|(x, y)| self.link_at(x, y));
        let changed = hover != self.hover;
        self.hover = hover;
        changed
    }

    /// The focused link let go; whether there was one.
    pub fn unfocus(&mut self) -> bool {
        self.focus.take().is_some()
    }

    /// Focuses the next or previous link, scrolling it into the body;
    /// with none focused, from the body: the first link whose line starts
    /// at its top or below, or the last ending at its bottom or above,
    /// else round. False when the page has no link.
    pub fn step_link(&mut self, forward: bool) -> bool {
        let Some(laid) = self.page.laid.as_deref() else {
            return false;
        };
        let links = laid.links();
        let count = links.len();
        if count == 0 {
            return false;
        }
        let (scale, page_top, y) = (self.scale, self.page_top, self.y);
        let height = i64::from(self.rect.height);
        let span = |line: u32| {
            line_span(laid, line, scale).map(|(top, bottom)| (page_top + top, page_top + bottom))
        };
        let next = match (self.focus, forward) {
            (None, true) => links
                .iter()
                .position(|&(_, line)| span(line).is_some_and(|(top, _)| top >= y))
                .unwrap_or(0),
            (None, false) => links
                .iter()
                .rposition(|&(_, line)| span(line).is_some_and(|(_, bottom)| bottom <= y + height))
                .unwrap_or(count - 1),
            (Some(at), true) => (at + 1) % count,
            (Some(at), false) => (at + count - 1) % count,
        };
        let shown = links.get(next).and_then(|&(_, line)| span(line));
        self.focus = Some(next);
        self.hover = None;
        self.stepped = true;
        self.named = false;
        if let Some((top, bottom)) = shown {
            if top < self.y {
                self.y = top;
            } else if bottom > self.y + height {
                self.y = bottom - height;
            }
            self.y = self.y.clamp(0, self.most());
        }
        true
    }

    /// Opens link `at` in `browser` (`td_ui::open::url`): what the status
    /// row says of it.
    pub fn open(&self, at: usize, browser: Option<&str>) -> Option<String> {
        let said = match self.target(at) {
            Ok(target) => match td_ui::open::url(&target, browser) {
                Ok(program) => td_alloc::format(format_args!("opened {target} with {program}")),
                Err(why) => td_alloc::format(format_args!("link not opened: {why}")),
            },
            Err(why) => td_alloc::format(format_args!("link not opened: {why}")),
        };
        said.ok()
    }

    /// Opens the link the status row names, as `open`: Return's. None
    /// when it names none.
    pub fn open_shown(&self, browser: Option<&str>) -> Option<String> {
        self.open(self.shown_link()?, browser)
    }

    /// A press at a window point: the link under it opened, as `open`,
    /// when it is the one the status row names; another link is named
    /// first and opened by the next press. None off every link.
    pub fn press(&mut self, x: i64, y: i64, browser: Option<&str>) -> Option<String> {
        let at = self.link_at(x, y)?;
        if self.shown_link() == Some(at) {
            return self.open(at, browser);
        }
        self.pointer = Some((x, y));
        self.stepped = false;
        self.named = true;
        self.hover = Some(at);
        td_alloc::format(format_args!("press again to open it")).ok()
    }

    /// What the status row says of the link under the pointer, else the
    /// focused one: where it goes, a long host cut in its middle, or why
    /// it does not.
    pub fn link_note(&self) -> Option<String> {
        match self.target(self.shown_link()?) {
            Ok(target) => shown_target(&target),
            Err(why) => td_alloc::format(format_args!("not opened: {why}")).ok(),
        }
    }

    /// The link the pointer is over, else the focused one.
    pub fn shown_link(&self) -> Option<usize> {
        self.hover.or(self.focus)
    }

    /// Where link `at` goes, as the browser is given it: its `href`
    /// resolved by td-url against the message's `<base href>` when that
    /// is an absolute URL, an `http` or `https` URL in td-url's printable
    /// ASCII, its fragment kept when that is printable ASCII without a
    /// backslash too. A link relative to no base, `//host` included, of
    /// another scheme or one td-url refuses says why it is not opened.
    pub fn target(&self, at: usize) -> Result<String, String> {
        let laid = self.page.laid.as_deref().ok_or("no page is laid out")?;
        let &(node, _) = laid.links().get(at).ok_or("no such link")?;
        let tree = laid.tree();
        let href = tree.href(node).ok_or("the link names no target")?.trim();
        let base = tree
            .base()
            .and_then(|base| td_url::Url::parse(base.trim()).ok());
        let (url, fragment) = match base {
            Some(base) => fetched(td_url::resolve(&base, href)?)?,
            None => {
                let https = td_url::Url::parse(NO_BASE_HTTPS)?;
                let http = td_url::Url::parse(NO_BASE_HTTP)?;
                let (url, fragment) = fetched(td_url::resolve(&https, href)?)?;
                let (other, _) = fetched(td_url::resolve(&http, href)?)?;
                if url.https != other.https {
                    return Err("a link relative to the message, which names no base".into());
                }
                (url, fragment)
            }
        };
        let fragment = fragment.filter(|f| f.bytes().all(|b| b.is_ascii_graphic() && b != b'\\'));
        match fragment {
            Some(fragment) => td_alloc::format(format_args!("{}#{fragment}", url.text()))
                .map_err(|_| "no memory for the link".into()),
            None => Ok(url.text()),
        }
    }

    /// The body painted: paper, the head, the rule, then the page, each
    /// clipped to its own band, so nothing the message lays out, wherever
    /// it places it, reaches the head or the rule.
    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(damage) = damage.intersection(self.rect) else {
            return;
        };
        sink(Draw {
            clip: damage,
            primitive: Primitive::Fill {
                rect: self.rect,
                color: PAPER,
            },
        });
        let top = self.rect.y - self.y;
        let rule = top + self.head_height + GAP * self.scale;
        let inset = INSET * self.scale;
        if let Some(line) = band(self.rect, rule, rule + RULE * self.scale) {
            let line = Rect {
                x: line.x + inset,
                width: u32::try_from(i64::from(line.width) - 2 * inset).unwrap_or(0),
                ..line
            };
            if let Some(clip) = damage.intersection(line) {
                sink(Draw {
                    clip,
                    primitive: Primitive::Fill {
                        rect: line,
                        color: BORDER,
                    },
                });
            }
        }
        let (Ok(mut measure), Ok(mut scratch)) =
            (self.measure.try_borrow_mut(), self.scratch.try_borrow_mut())
        else {
            return;
        };
        let parts = [
            (&self.head, top, band(self.rect, top, rule)),
            (
                &self.page,
                top + self.page_top,
                band(self.rect, top + self.page_top, i64::MAX),
            ),
        ];
        let link = self.shown_link();
        for (index, (page, y, clip)) in parts.into_iter().enumerate() {
            let (Some(laid), Some(clip)) = (page.laid.as_deref(), clip) else {
                continue;
            };
            // The focused or hovered link, drawn as selected, on the page.
            let marks = Marks {
                link: link
                    .filter(|_| index == 1)
                    .and_then(|at| laid.links().get(at))
                    .map(|&(node, _)| node),
                ..Marks::default()
            };
            let Some(damage) = damage.intersection(clip) else {
                continue;
            };
            // The window reserves a paint's glyph scratch so glyphs are
            // drawn by face and size; one it cannot have draws them as
            // they come.
            let _ = paint::reserve(&mut scratch, &self.budget);
            let frame = Frame {
                pane: clip,
                x: self.rect.x + inset,
                y,
                scale: self.scale,
            };
            paint::paint(
                laid,
                &mut measure,
                &frame,
                damage,
                &marks,
                &mut scratch,
                sink,
            );
        }
    }
}

/// A view's pages while its body is one, the pages asked of the page
/// thread, and the frame they were last refused for, which is not tried
/// again until the key, the body or the scale changes.
pub struct Held {
    paged: Option<Paged>,
    refused: Option<Tried>,
    asked: Option<Asked>,
    /// The frame whose pictures were refused beside its shown layout:
    /// its pages are made again whole for any more, the old let go first,
    /// rather than laid again to the same refusal.
    beside: Option<Tried>,
    /// How long a frame waits for the pages it asks (`IN_FRAME`).
    in_frame: Duration,
}

impl Default for Held {
    fn default() -> Self {
        Self {
            paged: None,
            refused: None,
            asked: None,
            beside: None,
            in_frame: IN_FRAME,
        }
    }
}

/// A key (and for `tried`, the pictures come) hashed, the body and the
/// scale.
type Tried = (u64, Rect, usize);

/// How long a frame waits for the pages it asked, so a message laid out
/// in less is shown in the frame that asked, not after a blank one.
#[cfg(not(test))]
const IN_FRAME: Duration = Duration::from_millis(40);
/// The tests see every answer come after the frame that asked, as a slow
/// page's does; `pages_come_in_the_frame_that_asked` waits.
#[cfg(test)]
const IN_FRAME: Duration = Duration::ZERO;

struct Asked {
    tried: Tried,
    /// The frame, its key hashed without the pictures come.
    frame: Tried,
    job: Asking,
}

/// Where the pages made again whole for the same frame were read: scroll,
/// focused link, whether a key stepped it, and the pointer.
type Place = (i64, Option<usize>, bool, Option<(i64, i64)>);

enum Asking {
    /// Pages made whole, the old let go, put back where they were read
    /// when they are the same frame's.
    Make(Pending<MakeJob, Option<Paged>>, Option<Place>),
    /// The shown page laid out again with `come` pictures.
    Lay(Pending<LayJob, Result<Page, Refusal>>, usize),
}

fn frame(key: &str, rect: Rect, scale: Scale) -> Tried {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish(), rect, scale.value())
}

/// Whether an `img` in `tree` names one of the pictures come.
fn names_any(tree: &Tree, come: &Come) -> bool {
    tree.pictures()
        .iter()
        .any(|&node| tree.src(node).is_some_and(|src| come.named(src).is_some()))
}

impl Held {
    pub fn get(&self) -> Option<&Paged> {
        self.paged.as_ref()
    }

    pub fn get_mut(&mut self) -> Option<&mut Paged> {
        self.paged.as_mut()
    }

    /// Let go, before a text is rendered or when the body is no page: a
    /// job asked is cancelled.
    pub fn clear(&mut self) {
        self.asked = None;
        self.paged = None;
        self.refused = None;
        self.beside = None;
    }

    /// Whether pages are asked of the page thread and not yet come.
    pub fn working(&self) -> bool {
        self.asked.is_some()
    }

    /// Asks the page thread for the pages for `key` at `rect` and `scale`,
    /// unless it holds them, has asked for them, or they were refused
    /// there. Pages for another frame are made whole, the old let go
    /// first; for pictures come since, the shown page is laid out again,
    /// shown meanwhile.
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        key: &str,
        head: &str,
        html: &str,
        rect: Rect,
        scale: Scale,
        faces: Option<&Faces>,
        pictures: Option<&Pictures>,
    ) {
        self.show_within(key, head, html, rect, scale, faces, BUDGET, pictures);
    }

    /// `show` within a budget of `limit` bytes.
    #[allow(clippy::too_many_arguments)]
    fn show_within(
        &mut self,
        key: &str,
        head: &str,
        html: &str,
        rect: Rect,
        scale: Scale,
        faces: Option<&Faces>,
        limit: usize,
        pictures: Option<&Pictures>,
    ) {
        let count = pictures.map_or(0, Pictures::count);
        let same = self.get().filter(|paged| paged.is(key, rect, scale));
        if same.is_some_and(|paged| paged.pictures == count) {
            return;
        }
        let laid = same.is_some();
        let frame = frame(key, rect, scale);
        // Where the frame's pages were read: the shown ones', or those a
        // remake of the same frame still asked carries.
        let place = same
            .map(|paged| (paged.y, paged.focus, paged.stepped, paged.pointer))
            .or_else(|| match self.asked.as_ref() {
                Some(Asked {
                    frame: asked,
                    job: Asking::Make(_, place),
                    ..
                }) if *asked == frame => *place,
                _ => None,
            });
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        count.hash(&mut hasher);
        let tried = (hasher.finish(), rect, scale.value());
        if self.refused == Some(tried) || self.asked.as_ref().is_some_and(|a| a.tried == tried) {
            return;
        }
        self.asked = None;
        let mut come = pictures.and_then(Pictures::come);
        if let Some(paged) = self.paged.as_mut().filter(|_| laid) {
            // Pictures come that no `img` of the shown page names (a
            // background's, a link's, or the page stepped down to its
            // text) change nothing it lays.
            let named = come
                .as_ref()
                .is_some_and(|come| names_any(&paged.page.tree, come));
            if !named {
                paged.pictures = count;
                return;
            }
        }
        if laid && self.beside != Some(frame) {
            if let Some(pending) = come.take().and_then(|come| self.lay(come, faces, scale)) {
                self.asked = Some(Asked {
                    tried,
                    frame,
                    job: Asking::Lay(pending, count),
                });
                self.wait_in_frame();
                // Refused beside within the frame's wait, the pages are
                // made whole now: no later frame is asked for.
                if self.asked.is_some() || self.beside != Some(frame) {
                    return;
                }
            }
        }
        self.paged = None;
        let markup = crate::compose::HTML_LIMITS.read(html);
        let job = (|| {
            Some(MakeJob {
                key: td_alloc::string(key).ok()?,
                head: td_alloc::string(head).ok()?,
                markup: td_alloc::string(markup).ok()?,
                cut: markup.len() < html.len(),
                rect,
                scale,
                faces: faces.cloned(),
                limit,
                pictures: come.or_else(|| pictures.and_then(Pictures::come)),
            })
        })();
        match job.and_then(|job| worker::ask(job, Job::Make)) {
            Some(pending) => {
                self.asked = Some(Asked {
                    tried,
                    frame,
                    job: Asking::Make(pending, place),
                });
                self.wait_in_frame();
            }
            None => self.refused = Some(tried),
        }
    }

    /// Waits `IN_FRAME` at most for the job just asked, and takes its
    /// answer if it came.
    fn wait_in_frame(&mut self) {
        if self.in_frame.is_zero() {
            return;
        }
        match self.asked.as_ref().map(|asked| &asked.job) {
            Some(Asking::Make(pending, _)) => pending.wait_for(self.in_frame),
            Some(Asking::Lay(pending, _)) => pending.wait_for(self.in_frame),
            None => return,
        }
        self.poll();
    }

    /// Asks for the shown page laid out again with the pictures `come`.
    fn lay(
        &self,
        come: Come,
        faces: Option<&Faces>,
        scale: Scale,
    ) -> Option<Pending<LayJob, Result<Page, Refusal>>> {
        let paged = self.paged.as_ref()?;
        let page = &paged.page;
        let old = page.laid.as_ref()?.try_clone().ok()?;
        let want = old.want;
        let again = Again {
            tree: page.tree.try_clone().ok()?,
            markup: page
                .markup
                .as_ref()
                .map(BudgetShared::try_clone)
                .transpose()
                .ok()?,
            sheets: page
                .sheets
                .as_ref()
                .map(BudgetShared::try_clone)
                .transpose()
                .ok()?,
            rung: page.rung,
            notes: page.notes,
            budget: page.budget.try_clone().ok()?,
            old: Some(old),
            keep: true,
            want,
            pictures: None,
        };
        let job = LayJob {
            again,
            pictures: come,
            faces: faces.cloned(),
            scale,
        };
        worker::ask(job, Job::Lay)
    }

    /// Takes what the page thread has answered: whether the pages shown
    /// changed, or what the status row says of them.
    pub fn poll(&mut self) -> bool {
        let Some(asked) = self.asked.as_ref() else {
            return false;
        };
        let (tried, frame) = (asked.tried, asked.frame);
        match &asked.job {
            Asking::Make(pending, place) => {
                let place = *place;
                let made = match pending.take() {
                    Taken::Waiting => return false,
                    Taken::Done(made) => made,
                    Taken::Lost => None,
                };
                self.asked = None;
                self.refused = made.is_none().then_some(tried);
                self.paged = made;
                // Made again whole for the same frame, the pages are put
                // back where they were read: the same HTML, the same links.
                if let Some((paged, (y, focus, stepped, pointer))) = self.paged.as_mut().zip(place)
                {
                    paged.y = y.clamp(0, paged.most());
                    let links = paged
                        .page
                        .laid
                        .as_deref()
                        .map_or(0, |laid| laid.links().len());
                    paged.focus = focus.filter(|&at| at < links);
                    paged.stepped = stepped && paged.focus.is_some();
                    paged.pointer = pointer;
                }
                true
            }
            Asking::Lay(pending, come) => {
                let come = *come;
                let laid = match pending.take() {
                    Taken::Waiting => return false,
                    Taken::Done(laid) => laid,
                    Taken::Lost => Err(Refusal::Limit("lost")),
                };
                self.asked = None;
                let shown = self.paged.as_mut().filter(|paged| {
                    laid.as_ref()
                        .is_ok_and(|page| BudgetShared::ptr_eq(&page.tree, &paged.page.tree))
                });
                match (shown, laid) {
                    (Some(paged), Ok(page)) => {
                        paged.page = page;
                        paged.pictures = come;
                        paged.y = paged.y.clamp(0, paged.most());
                    }
                    // Refused beside the shown layout, the pages are made
                    // again whole at the next frame, and for any picture
                    // after.
                    _ => self.beside = Some(frame),
                }
                true
            }
        }
    }

    /// Waits until the job asked, if one is, has answered, taking nothing.
    #[cfg(test)]
    pub fn wait_ready(&self) {
        match self.asked.as_ref().map(|asked| &asked.job) {
            Some(Asking::Make(pending, _)) => pending.wait_ready(),
            Some(Asking::Lay(pending, _)) => pending.wait_ready(),
            None => {}
        }
    }

    /// `poll`, waited for: the tests'.
    #[cfg(test)]
    pub fn settle(&mut self) {
        while let Some(asked) = self.asked.as_ref() {
            match &asked.job {
                Asking::Make(pending, _) => pending.wait_ready(),
                Asking::Lay(pending, _) => pending.wait_ready(),
            }
            self.poll();
        }
    }

    /// What the status row says of the pages.
    pub fn note(&self) -> Option<String> {
        match &self.paged {
            Some(paged) => paged.note(),
            None if self.asked.is_some() => td_alloc::string("Laying out the HTML").ok(),
            None if self.refused.is_some() => {
                td_alloc::format(format_args!("HTML could not be laid out; h: text")).ok()
            }
            None => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use td_ui::raster::{Composition, Surface};

    const BODY: Rect = Rect {
        x: 0,
        y: 24,
        width: 640,
        height: 400,
    };

    /// The pages painted over a surface, read back as td-ui reads a frame.
    struct Shown<'a>(&'a Paged);

    impl Composition for Shown<'_> {
        fn surface(&self) -> Surface {
            Surface::new(640, 480, Scale::default()).unwrap()
        }
        fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
            self.0.emit(damage, sink);
        }
    }

    fn text(paged: &Paged) -> String {
        td_ui::driven::text(&Shown(paged)).unwrap().2
    }

    fn made(head: &str, html: &str) -> Paged {
        Paged::make("m", head, html, BODY, Scale::default(), None, BUDGET, None).expect("made")
    }

    /// The head is td-mail's text above the page and the page is the
    /// message's HTML styled by its own sheet: what it hides is not shown,
    /// and its styles do not reach the head, another document.
    #[test]
    fn the_head_and_the_styled_page_are_shown_apart() {
        let html = "<style>.gone{display:none} *{color:red}</style>\
                    <p>Seen words<span class=gone>hidden words</span>\
                    <style>body{display:none}</style>";
        let paged = made("From: a@example.org\nSubject: Hi", html);
        let shown = text(&paged);
        assert!(shown.contains("From: a@example.org"), "{shown}");
        assert!(shown.contains("Subject: Hi"), "{shown}");
        assert!(!shown.contains("hidden words"), "{shown}");
        // The message's `body{display:none}` hid its own page.
        assert!(!shown.contains("Seen words"), "{shown}");
        assert_eq!(paged.note(), None);
        assert!(paged.head_height > 0);
        assert!(paged.page_top > paged.head_height);
        // Without the rule the page shows.
        let plain = made("Subject: Hi", "<p>Seen words");
        assert!(text(&plain).contains("Seen words"));
    }

    /// Between the head and the page td-mail draws its rule, and nothing
    /// the message lays out, however far up it places it, is drawn over
    /// the head or the rule.
    #[test]
    fn the_message_draws_below_the_rule_alone() {
        let html = "<p style='margin-top:-200px'>over the head</p>\
                    <p style='margin-top:200px'>in its place";
        let paged = made("Subject: Hi", html);
        let shown = text(&paged);
        assert!(shown.contains("Subject: Hi"), "{shown}");
        assert!(!shown.contains("over the head"), "{shown}");
        assert!(shown.contains("in its place"), "{shown}");
        let mut rules = Vec::new();
        let mut lowest_page_draw = i64::MAX;
        paged.emit(BODY, &mut |draw| match draw.primitive {
            Primitive::Fill { rect, color } if color == BORDER => rules.push(rect),
            _ if draw.clip.y > BODY.y => {
                lowest_page_draw = lowest_page_draw.min(draw.clip.y);
            }
            _ => {}
        });
        let [rule] = rules[..] else {
            panic!("one rule: {rules:?}");
        };
        assert_eq!(rule.y, BODY.y + paged.head_height + GAP);
        assert_eq!(rule.height, RULE as u32);
        assert!(lowest_page_draw > rule.y, "{lowest_page_draw}");
    }

    /// A head of any length scrolls away with the page, so all of it can
    /// be read, and the page keys and the ends move through both.
    #[test]
    fn a_long_head_scrolls_away_with_the_page() {
        let head: Vec<String> = (0..200).map(|n| format!("To: n{n}@example.org")).collect();
        let mut paged = made(&head.join("\n"), "<p>the body words");
        let top = text(&paged);
        assert!(top.contains("To: n0@example.org"), "{top}");
        assert!(!top.contains("the body words"), "{top}");
        assert!(paged.scroll_to(true));
        let end = text(&paged);
        assert!(end.contains("To: n199@example.org"), "{end}");
        assert!(end.contains("the body words"), "{end}");
        assert!(!end.contains("To: n0@example.org"), "{end}");
        assert!(paged.scroll_to(false));
        assert!(text(&paged).contains("To: n0@example.org"));
    }

    /// The page scrolls by lines, by bodies and to its ends, within its
    /// extent; a short message does not move.
    #[test]
    fn the_pages_scroll_within_their_extent() {
        let long: String = (0..300).map(|n| format!("<p>line {n}")).collect();
        let mut paged = made("Subject: long", &long);
        let line = paged.line;
        assert!(paged.scroll_lines(3));
        assert_eq!(paged.y, 3 * line);
        assert!(paged.scroll_pages(1));
        assert_eq!(paged.y, 3 * line + i64::from(BODY.height) - line);
        assert!(paged.scroll_to(true));
        let bottom = paged.y;
        assert_eq!(bottom, paged.most());
        assert!(!paged.scroll_lines(1), "already at the bottom");
        let end = text(&paged);
        assert!(end.contains("line 299"), "{end}");
        assert!(!end.contains("Subject: long"), "{end}");
        assert!(paged.scroll_lines(-1));
        assert!(paged.scroll_to(false));
        assert_eq!(paged.y, 0);
        assert!(!paged.scroll_lines(-1));
        assert!(paged.rows() > 1);
        let mut short = made("Subject: short", "<p>one line");
        assert!(!short.scroll_lines(1));
        assert!(!short.scroll_to(true));
    }

    /// The pages are the message's for its key, its body and its scale.
    #[test]
    fn the_pages_are_made_for_their_key_body_and_scale() {
        let paged = made("Subject: Hi", "<p>a");
        let one = Scale::default();
        assert!(paged.is("m", BODY, one));
        assert!(!paged.is("n", BODY, one));
        assert!(!paged.is("m", Rect { width: 600, ..BODY }, one));
        assert!(!paged.is("m", BODY, Scale::new(2).unwrap()));
    }

    /// A message too large for its budget steps down td-page's ladder and
    /// says so in the status row; at the least budget the pages are made
    /// in, every rung was refused and td-mail's notice is in its place.
    #[test]
    fn a_page_past_its_budget_steps_down_and_says_so() {
        let html: String = (0..400)
            .map(|n| format!("<div style='margin:{}px'><p>w{n}</div>", n % 9))
            .collect();
        let laid = |limit: usize| {
            Paged::make(
                "m",
                "Subject: big",
                &html,
                BODY,
                Scale::default(),
                None,
                limit,
                None,
            )
        };
        assert_eq!(laid(BUDGET).expect("made").note(), None);
        // The least budget that makes the pages at all, by halving.
        let (mut low, mut high) = (0, BUDGET);
        while high - low > 1024 {
            let mid = low + (high - low) / 2;
            if laid(mid).is_some() {
                high = mid;
            } else {
                low = mid;
            }
        }
        let least = laid(high).expect("made at the least");
        assert!(least.refused.is_some());
        let note = least.note().expect("a note");
        assert!(note.starts_with("HTML past td-mail's limits ("), "{note}");
        assert!(note.ends_with("; h: text"), "{note}");
        assert!(text(&least).contains("past what td-mail lays out"));
    }

    /// HTML past the input limit is cut at it, and the status row says so.
    #[test]
    fn html_past_the_input_limit_is_cut_and_says_so() {
        let most = crate::compose::HTML_LIMITS.input;
        let mut html = String::from("<p>");
        html.push_str(&"a ".repeat(most / 2));
        let whole = made("Subject: Hi", &html[..most]);
        assert_eq!(whole.note(), None);
        html.push_str("past the limit");
        let paged = made("Subject: Hi", &html);
        assert!(paged.cut);
        assert_eq!(
            paged.note().as_deref(),
            Some("HTML past 512 KiB not shown; h: text")
        );
    }

    /// A picture come for an `img`'s `cid:` URL is laid at its size, and
    /// one for a URL no `img` names lays nothing; the pages made again for
    /// a picture come keep their place and their focused link, which a
    /// pointer at rest over another link does not take back, and are
    /// made again for none else.
    #[test]
    fn a_cid_picture_come_is_laid_and_the_place_kept() {
        use crate::ui::pictures::tests::{come, png};
        let one = Scale::default();
        let html = format!(
            "{}<img src='cid:a%40x'><p><a href='https://e.example/1'>one</a> \
             <a href='https://e.example/2'>two</a> <a href='https://e.example/3'>three</a></p>\
             <img src='cid:b%40x'>",
            "<p>above</p>".repeat(40)
        );
        let make = |pictures| {
            Paged::make("m", "Subject: Hi", &html, BODY, one, None, BUDGET, pictures).expect("made")
        };
        let bare = make(None);
        let tall = come("a@x", &png(40, 3000));
        let laid = make(Some(&tall));
        assert!(
            laid.most() >= bare.most() + 2900,
            "{} {}",
            laid.most(),
            bare.most()
        );
        assert_eq!(laid.pictures, 1);
        let elsewhere = come("c@x", &png(40, 3000));
        assert_eq!(make(Some(&elsewhere)).most(), bare.most());

        let mut held = Held::default();
        held.show("m", "Subject: Hi", &html, BODY, one, None, None);
        held.settle();
        assert!(held.get_mut().expect("made").step_link(true));
        assert!(held.get_mut().expect("made").scroll_lines(-1));
        let y = held.get().expect("made").y;
        assert!(y > 0);
        held.show("m", "Subject: Hi", &html, BODY, one, None, Some(&tall));
        held.settle();
        let first: *const Paged = held.get().expect("made again");
        assert_eq!(
            held.get().map(|paged| (paged.y, paged.pictures)),
            Some((y, 1))
        );
        assert_eq!(held.get().and_then(Paged::shown_link), Some(0));
        held.show("m", "Subject: Hi", &html, BODY, one, None, Some(&tall));
        held.settle();
        assert_eq!(held.get().map(|p| p as *const Paged), Some(first));

        let paged = held.get_mut().expect("made");
        assert!(paged.step_link(true));
        let at = point_on(paged, 1).expect("link two shown");
        paged.hover(Some(at));
        assert_eq!(paged.shown_link(), Some(1));
        assert!(paged.step_link(true));
        assert_eq!(paged.shown_link(), Some(2));
        let both =
            crate::ui::pictures::tests::all_come(&[("a@x", &png(40, 3000)), ("b@x", &png(4, 4))]);
        held.show("m", "Subject: Hi", &html, BODY, one, None, Some(&both));
        held.settle();
        let paged = held.get_mut().expect("made again");
        assert_eq!(paged.pictures, 2);
        paged.hover(Some(at));
        assert_eq!(paged.shown_link(), Some(2));
    }

    /// Pages whose pictures do not fit beside them are laid without them
    /// at the rung they were made at, not stepped down the ladder: at the
    /// least budget a styled page is laid whole in, a picture come leaves
    /// it styled.
    #[test]
    fn a_page_refused_its_pictures_keeps_its_rung() {
        use crate::ui::pictures::tests::{come, png};
        let one = Scale::default();
        let html = format!(
            "{}<img src='cid:a%40x'>",
            "<div style='color:#123'>a</div>".repeat(400)
        );
        let tall = come("a@x", &png(40, 3000));
        let make = |limit, pictures| {
            Paged::make("m", "Subject: Hi", &html, BODY, one, None, limit, pictures)
        };
        let styled = |limit| make(limit, None).is_some_and(|paged| paged.note().is_none());
        let (mut low, mut high) = (1usize << 16, BUDGET);
        assert!(styled(high) && !styled(low));
        while high - low > 64 {
            let mid = low + (high - low) / 2;
            if styled(mid) {
                high = mid;
            } else {
                low = mid;
            }
        }
        let least = make(high, Some(&tall)).expect("made");
        assert_eq!(least.note(), None);
        let roomy = make(BUDGET, Some(&tall)).expect("made");
        assert!(roomy.most() > least.most());
    }

    /// Pages are made on the page thread: asked, the window holds none
    /// and says they are being laid out until the answer is taken; let go
    /// before it comes, as before a text renders, none is shown; and pages
    /// asked for another message take the place of those asked before.
    #[test]
    fn pages_are_made_on_the_page_thread_and_let_go_unseen() {
        let one = Scale::default();
        let mut held = Held::default();
        held.show("m", "Subject: Hi", "<p>a", BODY, one, None, None);
        assert!(held.get().is_none());
        assert!(held.working());
        assert_eq!(held.note().as_deref(), Some("Laying out the HTML"));
        held.settle();
        assert!(!held.working());
        assert!(held.get().is_some_and(|paged| paged.is("m", BODY, one)));
        assert_eq!(
            held.get().and_then(|paged| paged.made_on.as_deref()),
            Some("td-mail page")
        );
        assert_eq!(held.note(), None);

        held.clear();
        held.show("n", "Subject: Hi", "<p>b", BODY, one, None, None);
        held.clear();
        assert!(!held.working());
        assert!(!held.poll());
        assert!(held.get().is_none());

        held.show("n", "Subject: Hi", "<p>b", BODY, one, None, None);
        held.show("o", "Subject: Hi", "<p>c", BODY, one, None, None);
        held.settle();
        assert!(held.get().is_some_and(|paged| paged.is("o", BODY, one)));
    }

    /// Pictures come for the shown page lay it out again on the page
    /// thread while it is shown: the old layout stays until the new comes.
    /// Refused beside it, the pages are made again whole, the old let go
    /// first, rather than laid again and again.
    #[test]
    fn pictures_come_lay_the_shown_page_again_beside_it() {
        use crate::ui::pictures::tests::{come, png};
        let one = Scale::default();
        let html = format!(
            "<p><a href='https://e.example/'>top</a>{}<img src='cid:a%40x'>\
             <img src='cid:b%40x'>",
            "<div style='color:#123'>a</div>".repeat(400)
        );
        let tall = come("a@x", &png(40, 3000));
        let mut held = Held::default();
        held.show("m", "Subject: Hi", &html, BODY, one, None, None);
        held.settle();
        let bare = held.get().expect("made").most();
        held.show("m", "Subject: Hi", &html, BODY, one, None, Some(&tall));
        assert!(held.working());
        assert_eq!(held.get().map(|paged| paged.pictures), Some(0));
        held.settle();
        let laid = held.get().expect("laid again");
        assert_eq!(laid.pictures, 1);
        assert!(laid.most() >= bare + 2900);

        // At the least budget the page is made whole in, there is no room
        // for a second layout beside it.
        let styled = |limit| {
            Paged::make("m", "Subject: Hi", &html, BODY, one, None, limit, None)
                .is_some_and(|paged| paged.note().is_none())
        };
        let (mut low, mut high) = (1usize << 16, BUDGET);
        while high - low > 64 {
            let mid = low + (high - low) / 2;
            if styled(mid) {
                high = mid;
            } else {
                low = mid;
            }
        }
        let show = |held: &mut Held, pictures: Option<&Pictures>| {
            held.show_within("m", "Subject: Hi", &html, BODY, one, None, high, pictures);
        };
        let mut held = Held::default();
        show(&mut held, None);
        held.settle();
        // Read part way down, its link focused.
        let paged = held.get_mut().expect("made");
        assert!(paged.step_link(true));
        assert!(paged.scroll_lines(4));
        let place = (paged.y, paged.shown_link());
        assert!(place.0 > 0 && place.1.is_some());
        show(&mut held, Some(&tall));
        assert!(held.working());
        held.settle();
        assert_eq!(held.get().map(|paged| paged.pictures), Some(0));
        assert!(held.beside.is_some());
        show(&mut held, Some(&tall));
        assert!(held.get().is_none(), "made again whole, the old let go");
        held.settle();
        let made = held.get().expect("made again");
        assert_eq!((made.pictures, made.note()), (1, None));
        assert_eq!(
            (made.y, made.shown_link()),
            place,
            "put back where it was read"
        );
        show(&mut held, Some(&tall));
        assert!(!held.working());
        // Another picture: made whole at once, not laid again to the same
        // refusal.
        let both = crate::ui::pictures::tests::all_come(&[
            ("a@x", &png(40, 3000)),
            ("b@x", &png(40, 3000)),
        ]);
        show(&mut held, Some(&both));
        assert!(held.get().is_none(), "made whole, no lay asked");
        held.settle();
        assert_eq!(held.get().map(|paged| paged.pictures), Some(2));

        // A newer picture overtaking a remake still asked keeps the place
        // it carried.
        let mut held = Held::default();
        show(&mut held, None);
        held.settle();
        let paged = held.get_mut().expect("made");
        assert!(paged.step_link(true));
        assert!(paged.scroll_lines(4));
        let place = (paged.y, paged.shown_link());
        show(&mut held, Some(&tall));
        held.settle();
        show(&mut held, Some(&tall));
        assert!(held.get().is_none() && held.working(), "remade whole");
        show(&mut held, Some(&both));
        held.settle();
        let made = held.get().expect("made again");
        assert_eq!(made.pictures, 2);
        assert_eq!((made.y, made.shown_link()), place, "the place carried");

        // Refused beside within the frame's wait, the pages are made whole
        // in the same frame: none later is asked for.
        let mut held = Held {
            in_frame: Duration::from_secs(60),
            ..Held::default()
        };
        show(&mut held, None);
        assert!(held.get_mut().expect("made").scroll_lines(4));
        let y = held.get().expect("made").y;
        show(&mut held, Some(&tall));
        assert!(!held.working());
        assert!(held.beside.is_some());
        assert_eq!(
            held.get().map(|paged| (paged.pictures, paged.y)),
            Some((1, y))
        );
    }

    /// td-mail takes an `img`'s `src` alone: one with only a `srcset` is
    /// laid with no picture, though its `srcset` names one come, and
    /// names none of them.
    #[test]
    fn a_srcset_alone_lays_no_picture() {
        use crate::ui::pictures::tests::{come, png};
        let one = Scale::default();
        let html = "<p>above</p><img srcset='cid:a%40x 1x, https://e.example/a.png 2x' alt=x>\
                    <p>below</p>";
        let make = |pictures| {
            Paged::make("m", "Subject: Hi", html, BODY, one, None, BUDGET, pictures).expect("made")
        };
        let bare = make(None);
        let tall = come("a@x", &png(40, 3000));
        let laid = make(Some(&tall));
        assert_eq!(laid.most(), bare.most());
        let come = tall.come().expect("come");
        assert!(!names_any(&laid.page.tree, &come));
        assert!(set_of(&laid.page.tree, &laid.page.budget, &come).is_none());
    }

    /// td-page's `MAX_PICTURES` counts an `img` with only a `srcset`, so
    /// that many of them before a `cid:` picture leave it unlaid, as that
    /// many `src`s would; one fewer, and it is laid.
    #[test]
    fn srcset_images_take_the_picture_slots() {
        use crate::ui::pictures::tests::{come, png};
        let one = Scale::default();
        let most = td_page::inline::MAX_PICTURES;
        let page = |before: usize| {
            format!(
                "{}<img src='cid:a%40x'>",
                "<img srcset='https://e.example/x 2x'>".repeat(before)
            )
        };
        let tall = come("a@x", &png(40, 3000));
        let laid = |before: usize| {
            let html = page(before);
            let bare = Paged::make("m", "Subject: Hi", &html, BODY, one, None, BUDGET, None)
                .expect("made");
            let with = Paged::make(
                "m",
                "Subject: Hi",
                &html,
                BODY,
                one,
                None,
                BUDGET,
                Some(&tall),
            )
            .expect("made");
            // A 3000 px tall picture laid makes the page scroll.
            with.most() > bare.most()
        };
        assert!(laid(most - 1));
        assert!(!laid(most));
    }

    /// Pictures come that no `img` of the shown page names, as a table's
    /// background's, ask the page thread for nothing: the page stays as it
    /// is, where it is read.
    #[test]
    fn a_picture_no_img_names_lays_nothing() {
        use crate::ui::pictures::tests::{come, png};
        let one = Scale::default();
        let html = format!(
            "{}<table background='cid:a%40x'><tr><td>cell</table>",
            "<p>words</p>".repeat(300)
        );
        let mut held = Held::default();
        held.show("m", "Subject: Hi", &html, BODY, one, None, None);
        held.settle();
        assert!(held.get_mut().expect("made").scroll_lines(5));
        let shown: *const Paged = held.get().expect("made");
        let y = held.get().expect("made").y;
        let background = come("a@x", &png(4, 4));
        held.show(
            "m",
            "Subject: Hi",
            &html,
            BODY,
            one,
            None,
            Some(&background),
        );
        assert!(!held.working());
        assert_eq!(held.get().map(|paged| paged as *const Paged), Some(shown));
        assert_eq!(
            held.get().map(|paged| (paged.y, paged.pictures)),
            Some((y, 1))
        );
    }

    /// A frame waits a little for the pages it asked: a message laid out
    /// in less is shown in the frame that asked, not after a blank one.
    #[test]
    fn pages_come_in_the_frame_that_asked() {
        let one = Scale::default();
        let mut held = Held {
            in_frame: Duration::from_secs(60),
            ..Held::default()
        };
        held.show("m", "Subject: Hi", "<p>a", BODY, one, None, None);
        assert!(!held.working());
        assert!(held.get().is_some_and(|paged| paged.is("m", BODY, one)));
    }

    /// A view's pages are kept for the same key, body and scale, made
    /// again for another, and let go; pages refused are said, and not
    /// tried again for the same frame.
    #[test]
    fn held_pages_are_kept_remade_refused_and_let_go() {
        let one = Scale::default();
        let mut held = Held::default();
        held.show("m", "Subject: Hi", "<p>a", BODY, one, None, None);
        held.settle();
        let first: *const Paged = held.get().expect("made");
        held.show("m", "Subject: Hi", "<p>a", BODY, one, None, None);
        held.settle();
        assert_eq!(held.get().map(|p| p as *const Paged), Some(first));
        let wider = Rect { width: 600, ..BODY };
        held.show("m", "Subject: Hi", "<p>a", wider, one, None, None);
        held.settle();
        assert!(held.get().is_some_and(|paged| paged.is("m", wider, one)));
        held.clear();
        assert!(held.get().is_none());
        assert_eq!(held.note(), None);
        held.show_within("m", "Subject: Hi", "<p>a", BODY, one, None, 1024, None);
        held.settle();
        assert!(held.get().is_none());
        assert_eq!(
            held.note().as_deref(),
            Some("HTML could not be laid out; h: text")
        );
        // The same frame is not tried again, even with the room for it.
        held.show("m", "Subject: Hi", "<p>a", BODY, one, None, None);
        held.settle();
        assert!(held.get().is_none());
        held.show("m", "Subject: Hi", "<p>a", wider, one, None, None);
        held.settle();
        assert!(held.get().is_some());
        assert_eq!(held.note(), None);
    }

    /// The first window point over link `at`, scanning the body.
    fn point_on(paged: &Paged, at: usize) -> Option<(i64, i64)> {
        let bottom = BODY.y + i64::from(BODY.height);
        (BODY.y..bottom).step_by(2).find_map(|y| {
            (BODY.x..BODY.x + i64::from(BODY.width))
                .step_by(4)
                .find(|&x| paged.link_at(x, y) == Some(at))
                .map(|x| (x, y))
        })
    }

    /// A link is found under the pointer on the page alone, never on the
    /// head: a header that names a link is td-mail's text.
    #[test]
    fn a_link_is_found_on_the_page_and_not_the_head() {
        let paged = made(
            "Subject: see https://e.example/head",
            "<p>before <a href='https://e.example/one'>the first</a> after",
        );
        let (x, y) = point_on(&paged, 0).expect("the link is under some point");
        assert!(y >= BODY.y + paged.page_top);
        assert_eq!(paged.target(0).as_deref(), Ok("https://e.example/one"));
        for y in BODY.y..BODY.y + paged.page_top {
            assert_eq!(paged.link_at(x, y), None, "{x},{y}");
        }
        assert_eq!(paged.link_at(BODY.x - 1, y), None);
        // A link the message lays out over the head, clipped from view,
        // is not under the pointer there either.
        let over = made(
            "Subject: Hi",
            "<a href='https://e.example/up' style='display:block;margin-top:-40px'>up</a>",
        );
        assert!(over.target(0).is_ok());
        for y in BODY.y..BODY.y + over.page_top {
            for x in (BODY.x..BODY.x + 200).step_by(2) {
                assert_eq!(over.link_at(x, y), None, "{x},{y}");
            }
        }
    }

    /// Where a link goes is td-url's reading of its `href`: an absolute
    /// `http` or `https` URL, its fragment kept; a relative one against the
    /// message's absolute `<base href>` alone; any other scheme, a
    /// relative link with no base, or one td-url refuses, not opened.
    #[test]
    fn a_links_target_is_td_urls_reading_of_its_href() {
        let target = |html: &str| made("Subject: Hi", html).target(0);
        assert_eq!(
            target("<a href=' HTTPS://E.example/a b#part '>x</a>").as_deref(),
            Ok("https://e.example/a%20b#part")
        );
        assert_eq!(
            target("<base href='https://e.example/dir/'><a href='page'>x</a>").as_deref(),
            Ok("https://e.example/dir/page")
        );
        let relative = target("<a href='/page'>x</a>").unwrap_err();
        assert!(relative.contains("relative"), "{relative}");
        let relative = target("<base href='/dir/'><a href='page'>x</a>").unwrap_err();
        assert!(relative.contains("relative"), "{relative}");
        assert_eq!(
            target("<a href='mailto:me@e.example'>x</a>").unwrap_err(),
            "a mailto link"
        );
        assert_eq!(
            target("<a href='javascript:alert(1)'>x</a>").unwrap_err(),
            "a javascript link"
        );
        assert!(target("<a href='https://\u{e9}.example/'>x</a>").is_err());
        assert!(target("<a>no target</a>").is_err());
    }

    /// l and L step the focus through the page's links in order and round,
    /// scrolling the focused one into the body; the link focused or under
    /// the pointer is named in the status row and drawn as selected.
    #[test]
    fn the_focus_steps_through_the_links_and_shows_them() {
        let mut html = String::from("<p><a href='https://e.example/0'>zero</a>");
        for n in 1..200 {
            html.push_str(&format!("<p>line {n}"));
        }
        html.push_str("<p><a href='https://e.example/1'>one</a>");
        let mut paged = made("Subject: Hi", &html);
        let plain = draws(&paged);
        assert_eq!(paged.link_note(), None);
        assert!(paged.step_link(true));
        assert_eq!(paged.focus, Some(0));
        assert_eq!(paged.link_note().as_deref(), Some("https://e.example/0"));
        assert_ne!(
            draws(&paged),
            plain,
            "the focused link is drawn as selected"
        );
        assert!(paged.step_link(true));
        assert_eq!(paged.focus, Some(1));
        assert!(paged.y > 0, "scrolled to the second link");
        assert!(text(&paged).contains("one"));
        assert!(paged.step_link(true));
        assert_eq!(paged.focus, Some(0));
        assert!(text(&paged).contains("zero"), "and back to the first");
        assert!(paged.step_link(false));
        assert_eq!(paged.focus, Some(1));
        assert!(paged.unfocus());
        assert!(!paged.unfocus());
        // With none focused, back from the bottom is the last link shown.
        assert!(paged.step_link(false));
        assert_eq!(paged.focus, Some(1));
        // The pointer's link is named before the focused one.
        paged.scroll_to(false);
        let (x, y) = point_on(&paged, 0).unwrap();
        assert!(paged.hover(Some((x, y))));
        assert!(!paged.hover(Some((x, y))));
        assert_eq!(paged.link_note().as_deref(), Some("https://e.example/0"));
        assert!(paged.hover(None));
        assert_eq!(paged.link_note().as_deref(), Some("https://e.example/1"));
        let mut none = made("Subject: Hi", "<p>no link");
        assert!(!none.step_link(true));
        assert_eq!(none.focus, None);
    }

    /// The link under a point is the one painted there as the page is
    /// scrolled: scrolled down, it is found as far up; scrolled away, at
    /// no point of the body.
    #[test]
    fn a_link_is_found_where_the_page_is_scrolled_to() {
        let mut html = String::from("<p>above<p><a href='https://e.example/x'>there</a>");
        for n in 0..200 {
            html.push_str(&format!("<p>line {n}"));
        }
        let mut paged = made("Subject: Hi", &html);
        let (x, y) = point_on(&paged, 0).unwrap();
        assert!(paged.scroll_by(37));
        assert_eq!(paged.link_at(x, y - 37), Some(0));
        paged.scroll_to(true);
        assert_eq!(point_on(&paged, 0), None);
    }

    /// With no link focused, l starts at the first link whose line begins
    /// in the body or below, and L at the last ending in it or above, from
    /// wherever the body is scrolled.
    #[test]
    fn a_step_with_none_focused_starts_from_the_body() {
        let mut html = String::new();
        for at in 0..3 {
            html.push_str(&format!(
                "<p><a href='https://e.example/{at}'>link {at}</a>"
            ));
            for n in 0..100 {
                html.push_str(&format!("<p>line {n}"));
            }
        }
        let mut paged = made("Subject: Hi", &html);
        assert!(paged.step_link(true) && paged.step_link(true));
        assert_eq!(paged.focus, Some(1));
        let y = paged.y;
        assert!(y > 0);
        assert!(paged.unfocus());
        assert!(paged.step_link(true));
        assert_eq!(paged.focus, Some(1), "forward from a body showing link 1");
        assert!(paged.unfocus());
        assert!(paged.step_link(false));
        assert_eq!(paged.focus, Some(1), "back from a body showing link 1");
        assert_eq!(paged.y, y, "already shown, nothing scrolls");
    }

    /// After a key step the status row and Return speak of the focused
    /// link, not the one under a pointer at rest, until the pointer
    /// moves; a press on a link the row does not name names it, and the
    /// next press opens it.
    #[test]
    fn the_row_names_what_return_and_a_press_open() {
        let mut paged = made(
            "Subject: Hi",
            "<p><a href='https://e.example/first'>first</a>\
             <p><a href='https://e.example/big' style='display:block;height:300px'>big</a>",
        );
        let at = point_on(&paged, 1).unwrap();
        assert!(paged.hover(Some(at)));
        assert_eq!(paged.link_note().as_deref(), Some("https://e.example/big"));
        assert!(paged.step_link(true));
        assert_eq!(paged.focus, Some(0));
        paged.hover(Some(at));
        assert_eq!(
            paged.link_note().as_deref(),
            Some("https://e.example/first")
        );
        assert_eq!(
            paged.open_shown(Some("true")).as_deref(),
            Some("opened https://e.example/first with true")
        );
        // A press on the link under the pointer, not the one named.
        assert_eq!(
            paged.press(at.0, at.1, Some("true")).as_deref(),
            Some("press again to open it")
        );
        assert_eq!(paged.link_note().as_deref(), Some("https://e.example/big"));
        // Its button held, td-ui reports no pointer; the link stays named,
        // and is under the pointer again at the release.
        assert!(!paged.hover(None));
        assert_eq!(paged.link_note().as_deref(), Some("https://e.example/big"));
        paged.hover(Some(at));
        assert_eq!(paged.link_note().as_deref(), Some("https://e.example/big"));
        assert_eq!(
            paged.press(at.0, at.1, Some("true")).as_deref(),
            Some("opened https://e.example/big with true")
        );
        assert_eq!(paged.press(BODY.x - 1, at.1, Some("true")), None);
        // A moved pointer is followed again.
        assert!(paged.unfocus());
        assert!(paged.step_link(true));
        assert_eq!(paged.focus, Some(0));
        paged.hover(Some(at));
        assert_eq!(
            paged.link_note().as_deref(),
            Some("https://e.example/first")
        );
        paged.hover(Some((at.0 + 1, at.1)));
        assert_eq!(paged.link_note().as_deref(), Some("https://e.example/big"));
        assert_eq!(paged.hover, Some(1));
    }

    /// A host too long for the status row keeps only its end, where the
    /// name it is registered under is.
    #[test]
    fn a_long_host_keeps_its_end_in_the_row() {
        let host = format!("www.bank.example.{}.evil.example", "a".repeat(60));
        let mut paged = made(
            "Subject: Hi",
            &format!("<a href='https://{host}:8443/path'>x</a>"),
        );
        assert!(paged.step_link(true));
        let note = paged.link_note().unwrap();
        assert_eq!(
            paged.target(0).unwrap(),
            format!("https://{host}:8443/path")
        );
        assert!(note.starts_with("https://\u{2026}"), "{note}");
        assert!(!note.contains("bank"), "{note}");
        assert!(note.ends_with(".evil.example:8443/path"), "{note}");
        assert_eq!(
            note.chars().count(),
            "https://".len() + HOST_SHOWN + "/path".len()
        );
        let mut short = made("Subject: Hi", "<a href='https://e.example/p'>x</a>");
        assert!(short.step_link(true));
        assert_eq!(short.link_note().as_deref(), Some("https://e.example/p"));
    }

    /// A `//host` link takes its scheme from the base: with none it is
    /// relative and refused, as a path is; with one it is that scheme. An
    /// absolute link to the placeholder's name is no relative one; a
    /// fragment with a backslash, which the opener refuses, is dropped.
    #[test]
    fn a_scheme_relative_link_needs_a_base() {
        let target = |html: &str| made("Subject: Hi", html).target(0);
        for href in [
            "//evil.example/x",
            "\\\\evil.example\\x",
            "/\\evil.example/x",
        ] {
            let why = target(&format!("<a href='{href}'>x</a>")).unwrap_err();
            assert!(why.contains("relative"), "{href}: {why}");
        }
        assert_eq!(
            target("<base href='http://e.example/'><a href='//evil.example/x'>x</a>").as_deref(),
            Ok("http://evil.example/x")
        );
        assert_eq!(
            target("<a href='https://relative.invalid/x'>x</a>").as_deref(),
            Ok("https://relative.invalid/x")
        );
        assert_eq!(
            target("<a href='https://e.example/#a\\b'>x</a>").as_deref(),
            Ok("https://e.example/")
        );
    }

    /// A link not opened says why in the status row's words.
    #[test]
    fn a_link_not_opened_says_why() {
        let paged = made("Subject: Hi", "<a href='mailto:me@e.example'>x</a>");
        assert_eq!(
            paged.open(0, Some("/nonexistent/browser")).as_deref(),
            Some("link not opened: a mailto link")
        );
        let paged = made("Subject: Hi", "<a href='https://e.example/'>x</a>");
        let said = paged.open(0, Some("/nonexistent/browser")).unwrap();
        assert!(said.starts_with("link not opened: "), "{said}");
        assert_eq!(paged.open_shown(None), None, "no link named");
    }

    /// Every draw the pages make, as text to compare.
    fn draws(paged: &Paged) -> String {
        let mut out = String::new();
        paged.emit(BODY, &mut |draw| out.push_str(&format!("{draw:?}\n")));
        out
    }

    /// Host measurement (README.md, "Memory"), release build: each hostile
    /// shape as a message whose body is HTML alone, opened in its view,
    /// which keeps its text, lines and links, made into its pages for a
    /// body at 2880x1800 (scale two), and then quoted for a reply, the
    /// view letting its pages go first: the pages' budget use and the
    /// resident peak over the whole, reset before each. `TD_MAIL_SHAPE`
    /// picks one shape a run, so none reuses another's freed heap, and
    /// `TD_MAIL_SANS` and `TD_MAIL_SERIF` name the prose faces' folders.
    #[test]
    #[ignore = "host measurement, release build"]
    fn measure_pages() {
        use crate::backend::BackendResponse;
        use crate::jmap::types::{BodyPart, BodyValue, Email};
        use crate::ui::input::Key;
        use crate::ui::views::{email_view::EmailView, View};
        use std::collections::HashMap;

        fn resident() -> usize {
            let status = std::fs::read_to_string("/proc/self/status").unwrap();
            let line = status.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
            line.split_whitespace()
                .nth(1)
                .unwrap()
                .parse::<usize>()
                .unwrap()
                * 1024
        }
        let most = crate::compose::HTML_LIMITS.input;
        let fill_to = |most: usize, head: &str, unit: &str| {
            let mut out = String::from(head);
            while out.len() + unit.len() <= most {
                out.push_str(unit);
            }
            out
        };
        let fill = |head: &str, unit: &str| fill_to(most, head, unit);
        let email = |html: &str| {
            let part = BodyPart {
                part_id: "1".to_string(),
                blob_id: None,
                r#type: Some("text/html".to_string()),
                name: None,
                size: None,
                cid: None,
            };
            let mut values = HashMap::new();
            values.insert(
                "1".to_string(),
                BodyValue {
                    value: html.to_string(),
                    is_encoding_problem: false,
                    is_truncated: false,
                },
            );
            Email {
                id: "e1".to_string(),
                thread_id: None,
                from: None,
                to: None,
                cc: None,
                reply_to: None,
                subject: Some("shape".to_string()),
                received_at: None,
                sent_at: None,
                preview: None,
                text_body: Some(vec![part.clone()]),
                html_body: Some(vec![part]),
                body_values: values,
                keywords: HashMap::new(),
                mailbox_ids: HashMap::new(),
                message_id: None,
                references: None,
                attachments: None,
                extra: HashMap::new(),
            }
        };
        // Ordinary mail: a sheet, and articles laid out by nested tables
        // and inline styles, as newsletters are.
        let mut newsletter = String::from(
            "<html><head><style>body{margin:0;background:#eee}.btn{color:#fff;\
             background:#36c;padding:8px 16px;border-radius:4px}h2{font-size:20px}\
             </style></head><body><table width=100% cellpadding=0 cellspacing=0>\
             <tr><td align=center><table width=600 style='background:#fff'>",
        );
        let mut n = 0;
        while newsletter.len() < 100 << 10 {
            newsletter.push_str(&format!(
                "<tr><td style='padding:16px 24px;font-family:Arial,sans-serif;\
                 font-size:15px;line-height:22px;color:#333'><h2>Article {n}</h2>\
                 <table width=100%><tr><td width=120><img src='https://e.example/{n}.png' \
                 width=120 height=80 alt='picture {n}'></td><td style='padding-left:12px'>\
                 Some words about the article, a sentence or two of it, with \
                 <a href='https://e.example/a/{n}' style='color:#36c'>a link</a>.</td>\
                 </tr></table><p><a class=btn href='https://e.example/r/{n}'>Read more</a>\
                 </td></tr>"
            ));
            n += 1;
        }
        newsletter.push_str("</table></td></tr></table></body></html>");
        let link = format!("<a href=\"https://e.example/{}\">", "x".repeat(16 << 10));
        let quoted = format!("{link}{}", "<blockquote>".repeat(62));
        let nested_tables = format!(
            "{}w{}",
            "<table><tr><td>".repeat(12),
            "</td></tr></table>".repeat(12)
        );
        let nested_blocks = format!(
            "{}w{}",
            "<span style='display:inline-block'>".repeat(40),
            "</span>".repeat(40)
        );
        let shapes = [
            ("quoted letters in a link", fill(&quoted, "<p>x</p>")),
            ("open quoted letters in a link", fill(&quoted, "<p>x")),
            (
                "floats",
                fill("", "<div style='float:left;width:3px'>w</div>"),
            ),
            (
                "inline blocks",
                fill("", "<span style='display:inline-block'>w</span>"),
            ),
            ("pictures' alt", fill("", "<img alt=x>")),
            ("empty paragraphs", fill("", "<p>")),
            (
                "class floats",
                fill(
                    "<style>.f{float:left;width:3px}</style>",
                    "<div class=f>w</div>",
                ),
            ),
            ("nested tables", fill("", &nested_tables)),
            ("nested inline blocks", fill("", &nested_blocks)),
            (
                "styled divs",
                fill(
                    "<style>div{margin:1px;border:1px solid}</style>",
                    "<div>w</div>",
                ),
            ),
            ("nested divs", fill("", "<div>")),
            ("a table", fill("<table>", "<tr><td>a<td>b<td>c")),
            ("prose", fill("<p>", "lorem ipsum dolor sit amet ")),
            (
                "inline styles",
                fill("", "<span style='color:red;margin:1px'>w</span>"),
            ),
            ("a 100 KiB newsletter", newsletter),
            // The costliest that still fit the budget.
            (
                "styled divs within the budget",
                fill_to(
                    300 << 10,
                    "<style>div{margin:1px;border:1px solid}</style>",
                    "<div>w</div>",
                ),
            ),
            (
                "quoted letters within the budget",
                fill_to(220 << 10, &quoted, "<p>x</p>"),
            ),
        ];
        let body = Rect {
            x: 0,
            y: 48,
            width: 2880,
            height: 1800 - 96,
        };
        let scale = Scale::new(2).unwrap();
        // The prose faces from `TD_MAIL_SANS` and `TD_MAIL_SERIF`, two
        // directories, when they are set: what they hold, read.
        let faces = std::env::var_os("TD_MAIL_SANS").map(|sans| {
            let serif = std::env::var_os("TD_MAIL_SERIF").unwrap();
            let _ = std::fs::write("/proc/self/clear_refs", "5");
            let before = resident();
            let faces = td_ui::pinned_face::prose_from(sans.as_ref(), serif.as_ref()).unwrap();
            println!(
                "the prose faces: {:.1} MB",
                resident().saturating_sub(before) as f64 / 1e6
            );
            faces
        });
        // One shape a process, `TD_MAIL_SHAPE` its index, so none reuses
        // another's freed heap.
        let only: Option<usize> = std::env::var("TD_MAIL_SHAPE")
            .ok()
            .map(|v| v.parse().unwrap());
        for (index, (name, html)) in shapes.iter().enumerate() {
            if only.is_some_and(|only| only != index) {
                continue;
            }
            let message = email(html);
            let _ = std::fs::write("/proc/self/clear_refs", "5");
            let before = resident();
            let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel();
            let mut view = EmailView::new(
                cmd_tx,
                "me@example.com".to_string(),
                "e1".to_string(),
                Vec::new(),
                0,
                false,
                Vec::new(),
                "Archive".to_string(),
                "Trash".to_string(),
                None,
            );
            view.on_response(&BackendResponse::EmailBody {
                id: "e1".to_string(),
                result: Box::new(Ok(message.clone())),
            });
            let viewed = resident();
            let making = std::time::Instant::now();
            view.lay_out_page(Some(body), scale, faces.as_ref());
            view.settle_work();
            let paged = view.page().and_then(Held::get).expect("pages");
            let (used, note) = (paged.budget.used(), paged.note());
            let made = resident();
            let laid = making.elapsed();
            let started = std::time::Instant::now();
            view.handle_key(Key::Char('r'), 10);
            view.on_response(&BackendResponse::EmailForReply {
                id: "e1".to_string(),
                result: Box::new(Ok(message.clone())),
            });
            assert!(view.page().and_then(Held::get).is_none());
            let reply = view.take_pending_action().expect("a draft");
            let took = started.elapsed();
            let peak = resident();
            println!(
                "{name}: view {:.1} MB, then the pages {:.1} MB ({:.1} MiB of the budget, \
                 {laid:.0?}), then the reply {:.1} MB ({took:.0?}): {:.1} MB in all, {:?}",
                viewed.saturating_sub(before) as f64 / 1e6,
                made.saturating_sub(viewed) as f64 / 1e6,
                used as f64 / 1048576.0,
                peak.saturating_sub(made) as f64 / 1e6,
                peak.saturating_sub(before) as f64 / 1e6,
                note,
            );
            drop((reply, view));
        }
    }
}
