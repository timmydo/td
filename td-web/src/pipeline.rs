//! A page made on the render thread (DESIGN.md §10, "Pipeline and
//! threads" and "Cuts"): parsed, its sheets compiled, cascaded and laid
//! out, stepping down the cuts' ladder on a refusal; and laid out again
//! for a new viewport, scale, face or `F9`.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::fonts::Measure;
use crate::memory;
use crate::page::{Laid, Styled, Want};
use crate::sheets::{self, Omitted};
use crate::tree::{self, Source, Tree, TEXT_PREFIX};
use td_alloc::{Budget, BudgetError, BudgetShared, BudgetString};
use td_html::extract::{Extractor, Status};
use td_layout::{Error, Limits, MAX_INPUT, MAX_PAGE_BYTES};

/// The most lines a text page lays at its least width.
pub const MAX_TEXT_LINES: usize = 262_144;
/// The least viewport, in CSS px: `MIN_WRAP_EMS` ems of td-ui's `SIZE`.
pub const LEAST_VIEWPORT: f64 = 128.0;
/// The most bytes a text page holds, its prefix among them.
pub const MAX_TEXT: usize = 4 << 20;

/// Which rung of the cuts' ladder a page stands on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rung {
    /// With the page's own sheets, or a text page or td-web's own.
    Own,
    /// The user-agent sheets alone and no `style` attributes.
    UserAgent,
    /// The page's text, extracted from its markup.
    Text,
}

/// Of a text page cut by the lines it lays: its source lines shown, of
/// how many, and whether the last is shown in part.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Shown {
    pub lines: usize,
    pub of: usize,
    pub part: bool,
}

/// What the status row says of how a page was made.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Notes {
    /// The page's sheets left out or refused.
    pub omitted: usize,
    /// Its styles refused, by this limit (rung 2).
    pub styles: Option<&'static str>,
    /// Too large to lay out, by this limit (rung 3).
    pub text: Option<&'static str>,
    /// A text page cut.
    pub shown: Option<Shown>,
}

/// Why a step gave no page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    Cancelled,
    Limit(&'static str),
}

impl From<Error> for Refusal {
    fn from(error: Error) -> Self {
        match error {
            Error::Cancelled => Self::Cancelled,
            error => Self::Limit(limit(error)),
        }
    }
}

impl From<BudgetError> for Refusal {
    fn from(_: BudgetError) -> Self {
        Self::Limit("memory")
    }
}

impl From<tree::Refusal> for Refusal {
    fn from(refusal: tree::Refusal) -> Self {
        match refusal {
            tree::Refusal::Parse(name) => Self::Limit(name),
            tree::Refusal::Memory => Self::Limit("memory"),
        }
    }
}

/// The limit a td-layout refusal names, for the status row.
pub fn limit(error: Error) -> &'static str {
    match error {
        Error::Limit(name) => name,
        Error::Css(td_css::Error::Limit(name)) => name,
        Error::Css(td_css::Error::Memory(_)) | Error::Memory(_) => "memory",
        Error::Css(_) => "the page's styles",
        Error::Tree(_) => "td-dom's index",
        Error::Invalid => "an invalid request",
        Error::Cancelled => "cancelled",
    }
}

fn css(error: td_css::Error) -> Refusal {
    Refusal::from(Error::Css(error))
}

/// A page as the window thread holds it.
pub struct Page {
    pub tree: BudgetShared<Tree>,
    /// A markup page's markup, which rung 3 reads after a relayout let
    /// the old layout go first.
    pub markup: Option<BudgetShared<BudgetString>>,
    /// None while a relayout that let it go first runs.
    pub laid: Option<BudgetShared<Laid>>,
    pub rung: Rung,
    pub notes: Notes,
    /// The page's budget, its terms' parent.
    pub budget: Budget,
    /// Told the thread the page is dropped on, in tests: held for its
    /// drop.
    #[cfg(test)]
    #[allow(dead_code)]
    pub recorder: Option<Recorder>,
}

/// Tells the thread it is dropped on: a page's, in tests.
#[cfg(test)]
pub struct Recorder(pub std::sync::mpsc::Sender<std::thread::ThreadId>);

#[cfg(test)]
impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.0.send(std::thread::current().id());
    }
}

/// Where a job tests its flag (DESIGN.md §10, "Pipeline and threads").
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    Parse,
    Compile,
    Cascade,
    Layout,
    /// A text page's lines counted.
    Count,
    /// Rung 3's text extracted.
    Extract,
    /// A refusal, before it is taken or stepped down from.
    Down,
}

/// Whether `cancel` is set: tested after each step, since td-layout
/// tests its limits first, and before a refusal is taken or stepped down
/// from, since a step refused by a limit may have been cancelled too.
fn stopped(cancel: &AtomicBool, step: Step) -> Result<(), Refusal> {
    #[cfg(test)]
    tests::reach(cancel, step, tests::When::At);
    if cancel.load(Ordering::SeqCst) {
        return Err(Refusal::Cancelled);
    }
    #[cfg(test)]
    tests::reach(cancel, step, tests::When::After);
    #[cfg(not(test))]
    let _ = step;
    Ok(())
}

/// `tree` cascaded for `want` with the page's own sheets and attributes
/// when `own`, compiled in the sheets term and dropped once cascaded, the
/// styled document's construction within the page term less `kept`.
fn style(
    tree: &BudgetShared<Tree>,
    own: bool,
    want: Want,
    input_bytes: usize,
    kept: usize,
    cancel: &AtomicBool,
    page: &Budget,
) -> Result<BudgetShared<Styled>, Refusal> {
    let terms = page.child(memory::SHEETS.saturating_add(Budget::metadata_bytes()))?;
    let environment = sheets::environment(want.width, want.height, u32::from(want.scale));
    let (set, omitted) = sheets::compile(tree, own, &environment, &terms).map_err(css)?;
    stopped(cancel, Step::Compile)?;
    let coloured = own && sheets::coloured(tree);
    let limits = Limits {
        input: MAX_INPUT,
        bytes: MAX_PAGE_BYTES.saturating_sub(kept),
        style_attributes: own,
        cancel: Some(cancel),
        ..Limits::default()
    };
    let styled = Styled::new(
        tree.try_clone()?,
        &set,
        input_bytes,
        limits,
        omitted,
        coloured,
        page,
    )?;
    drop(set);
    stopped(cancel, Step::Cascade)?;
    Ok(BudgetShared::new(page, styled)?)
}

fn lay(
    styled: BudgetShared<Styled>,
    measure: &mut Measure,
    want: Want,
    retained: usize,
    cancel: &AtomicBool,
    page: &Budget,
) -> Result<BudgetShared<Laid>, Refusal> {
    let laid = Laid::new(styled, measure, want, retained, cancel, page)?;
    stopped(cancel, Step::Layout)?;
    Ok(BudgetShared::new(page, laid)?)
}

/// The page's own sheets' omissions, for its notes.
fn omitted(laid: &Laid) -> Omitted {
    laid.styled().omitted
}

/// A markup page from `markup`, stepping down the ladder on a refusal;
/// `Err(Cancelled)` once `cancel` is seen, `Err(Limit)` when rung 4's
/// static notice is all there is.
pub fn markup_page(
    markup: BudgetShared<BudgetString>,
    want: Want,
    measure: &mut Measure,
    cancel: &AtomicBool,
    page: &Budget,
) -> Result<Page, Refusal> {
    let tree = match Tree::parse(&markup, Source::Markup, page) {
        Ok(tree) => BudgetShared::new(page, tree)?,
        Err(refusal) => {
            stopped(cancel, Step::Down)?;
            return extracted(markup, Refusal::from(refusal), want, measure, cancel, page);
        }
    };
    stopped(cancel, Step::Parse)?;
    styled_page(tree, Some(markup), want, measure, cancel, page)
}

/// Rungs 1 and 2 of a parsed tree, then rung 3.
fn styled_page(
    tree: BudgetShared<Tree>,
    markup: Option<BudgetShared<BudgetString>>,
    want: Want,
    measure: &mut Measure,
    cancel: &AtomicBool,
    page: &Budget,
) -> Result<Page, Refusal> {
    let input = markup.as_ref().map_or(0, |markup| markup.len());
    let mut notes = Notes::default();
    if want.styles {
        let own = style(&tree, true, want, input, 0, cancel, page)
            .and_then(|styled| lay(styled, measure, want, 0, cancel, page));
        match own {
            Ok(laid) => {
                notes.omitted = omitted(&laid).count();
                return Ok(Page {
                    tree,
                    markup,
                    laid: Some(laid),
                    rung: Rung::Own,
                    notes,
                    budget: page.try_clone()?,
                    #[cfg(test)]
                    recorder: None,
                });
            }
            Err(Refusal::Cancelled) => return Err(Refusal::Cancelled),
            Err(Refusal::Limit(name)) => {
                stopped(cancel, Step::Down)?;
                notes.styles = Some(name);
            }
        }
    }
    user_agent(tree, markup, notes, want, measure, cancel, page)
}

/// Rung 2, then rung 3.
fn user_agent(
    tree: BudgetShared<Tree>,
    markup: Option<BudgetShared<BudgetString>>,
    notes: Notes,
    want: Want,
    measure: &mut Measure,
    cancel: &AtomicBool,
    page: &Budget,
) -> Result<Page, Refusal> {
    let input = markup.as_ref().map_or(0, |markup| markup.len());
    let laid = style(&tree, false, want, input, 0, cancel, page)
        .and_then(|styled| lay(styled, measure, want, 0, cancel, page));
    match laid {
        Ok(laid) => Ok(Page {
            tree,
            markup,
            laid: Some(laid),
            rung: Rung::UserAgent,
            notes,
            budget: page.try_clone()?,
            #[cfg(test)]
            recorder: None,
        }),
        Err(Refusal::Cancelled) => Err(Refusal::Cancelled),
        Err(refusal) => {
            drop(tree);
            stopped(cancel, Step::Down)?;
            match markup {
                Some(markup) => extracted(markup, refusal, want, measure, cancel, page),
                None => Err(refusal),
            }
        }
    }
}

/// What td-html's bounded extractor gives of `markup`, at most `most`
/// bytes.
fn extract(markup: &str, most: usize, memory: &Budget) -> Result<BudgetString, Refusal> {
    let mut out = td_alloc::BudgetString::with_capacity(memory, markup.len().min(most))?;
    let mut extractor = Extractor::new();
    let mut scalars = markup.chars().peekable();
    // Each poll consumes a scalar or yields one; this bounds a misstep.
    let mut steps = markup.len().saturating_mul(8).saturating_add(4096);
    loop {
        steps = steps
            .checked_sub(1)
            .ok_or(Refusal::Limit("the extractor"))?;
        let input = scalars.peek().copied();
        let progress = extractor
            .poll(input, input.is_none())
            .map_err(|_| Refusal::Limit("the extractor"))?;
        if progress.consumed {
            scalars.next();
        }
        if let Some(scalar) = progress.scalar {
            if out.len().saturating_add(scalar.len_utf8()) > most {
                break;
            }
            out.try_push(scalar)?;
        }
        if progress.status == Status::Complete {
            break;
        }
    }
    Ok(out)
}

/// Rung 3: the page's text from its markup, laid as a text page; the
/// refusal that brought it here named.
fn extracted(
    markup: BudgetShared<BudgetString>,
    why: Refusal,
    want: Want,
    measure: &mut Measure,
    cancel: &AtomicBool,
    page: &Budget,
) -> Result<Page, Refusal> {
    let text = extract(&markup, MAX_TEXT.saturating_sub(TEXT_PREFIX.len()), page)?;
    stopped(cancel, Step::Extract)?;
    let mut made = text_page(&text, want, measure, cancel, page)?;
    drop(text);
    made.rung = Rung::Text;
    made.markup = Some(markup);
    made.notes.text = match why {
        Refusal::Limit(name) => Some(name),
        Refusal::Cancelled => None,
    };
    Ok(made)
}

/// Lines in `text`: one more than its newlines, none when empty.
fn lines_in(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.bytes().filter(|&b| b == b'\n').count() + usize::from(!text.ends_with('\n'))
    }
}

/// `text` as a text page, cut where it would lay past `MAX_TEXT_LINES`
/// lines in the least viewport (DESIGN.md §10, "Cuts").
pub fn text_page(
    text: &str,
    want: Want,
    measure: &mut Measure,
    cancel: &AtomicBool,
    page: &Budget,
) -> Result<Page, Refusal> {
    let markup = tree::text_markup(text, MAX_TEXT, page)?;
    let tree = BudgetShared::new(page, Tree::parse(&markup, Source::Text, page)?)?;
    drop(markup);
    stopped(cancel, Step::Parse)?;
    let (tree, shown) = fitted(tree, None, want, measure, cancel, page)?;
    let input = node_text(&tree).len() + TEXT_PREFIX.len();
    let styled = style(&tree, false, want, input, 0, cancel, page)?;
    let laid = lay(styled, measure, want, 0, cancel, page)?;
    Ok(Page {
        tree,
        markup: None,
        laid: Some(laid),
        rung: Rung::Own,
        notes: Notes {
            shown,
            ..Notes::default()
        },
        budget: page.try_clone()?,
        #[cfg(test)]
        recorder: None,
    })
}

/// A text page's one text node's text.
fn node_text(tree: &Tree) -> &str {
    let document = tree.document();
    let mut at = Some(document.root());
    while let Some(id) = at {
        if let Some(text) = document.node(id).and_then(|node| node.text()) {
            return text;
        }
        at = tree.next(id, false);
    }
    ""
}

/// A text page's tree counted at the least viewport with `measure`, and
/// built again from its text up to the cut when it passes; `shown`
/// carries an earlier cut's count of the source lines.
fn fitted(
    tree: BudgetShared<Tree>,
    shown: Option<Shown>,
    want: Want,
    measure: &mut Measure,
    cancel: &AtomicBool,
    page: &Budget,
) -> Result<(BudgetShared<Tree>, Option<Shown>), Refusal> {
    let text = node_text(&tree);
    let input = text.len() + TEXT_PREFIX.len();
    let styled = style(&tree, false, want, input, 0, cancel, page)?;
    measure.forget();
    let fit = td_layout::text::fit(
        styled.styled(),
        measure,
        LEAST_VIEWPORT,
        MAX_TEXT_LINES,
        Some(cancel),
    )?;
    drop(styled);
    stopped(cancel, Step::Count)?;
    let Some(end) = fit.end else {
        return Ok((tree, shown));
    };
    let kept = text.get(..end).unwrap_or("");
    let of = shown.map_or_else(|| lines_in(text), |shown| shown.of);
    let part = !kept.ends_with('\n') && text.get(end..).is_some_and(|rest| !rest.starts_with('\n'));
    let shown = Shown {
        lines: lines_in(kept),
        of,
        part,
    };
    let mut markup = td_alloc::BudgetString::with_capacity(page, TEXT_PREFIX.len() + kept.len())?;
    markup.try_push_str(TEXT_PREFIX)?;
    markup.try_push_str(kept)?;
    let cut = Tree::parse(&markup, Source::Text, page)?;
    drop(markup);
    drop(tree);
    Ok((BudgetShared::new(page, cut)?, Some(shown)))
}

/// A page td-web writes from `text`.
pub fn own_page(
    text: &str,
    want: Want,
    measure: &mut Measure,
    cancel: &AtomicBool,
    page: &Budget,
) -> Result<Page, Refusal> {
    let markup = tree::own_markup(text, page)?;
    let tree = BudgetShared::new(page, Tree::parse(&markup, Source::Own, page)?)?;
    let styled = style(&tree, false, want, markup.len(), 0, cancel, page)?;
    drop(markup);
    let laid = lay(styled, measure, want, 0, cancel, page)?;
    Ok(Page {
        tree,
        markup: None,
        laid: Some(laid),
        rung: Rung::Own,
        notes: Notes::default(),
        budget: page.try_clone()?,
        #[cfg(test)]
        recorder: None,
    })
}

/// What a relayout is given: the page's parts, and the old layout kept
/// beside the new one, or let go first.
pub struct Again {
    pub tree: BudgetShared<Tree>,
    pub markup: Option<BudgetShared<BudgetString>>,
    pub rung: Rung,
    pub notes: Notes,
    pub budget: Budget,
    /// The layout shown, kept beside the new one when the window decided
    /// it fits; else given up with the request and dropped here first.
    pub old: Option<BudgetShared<Laid>>,
    pub keep: bool,
    pub want: Want,
}

/// What `again` must redo for `want` over a layout made for `was`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Redo {
    Nothing,
    Layout,
    Cascade,
}

pub fn redo(was: Want, want: Want) -> Redo {
    if was.width != want.width || was.scale != want.scale || was.styles != want.styles {
        Redo::Cascade
    } else if was.height != want.height || was.body != want.body {
        Redo::Layout
    } else {
        Redo::Nothing
    }
}

/// The declared bytes a relayout keeping `old` beside it must fit: the
/// tree, the styled document and output kept, and the new ones, each
/// predicted at the old one's size, an output growing as the viewport
/// narrows. Whether it fits the page term and the root's headroom.
pub fn fits_beside(old: &Laid, want: Want, headroom: usize) -> bool {
    let styled = old.styled();
    let held = Held {
        tree: styled.styled().charges().tree,
        styled: styled.bytes(),
        output: old.bytes(),
    };
    let cascade = redo(old.want, want) == Redo::Cascade;
    let narrower = old.want.width / want.width.max(1.0);
    beside(held, narrower, cascade, headroom)
}

/// What a page holds of the terms a relayout's prediction reads.
#[derive(Clone, Copy, Debug)]
struct Held {
    tree: usize,
    styled: usize,
    output: usize,
}

/// `fits_beside` over the bytes `held`, the old width over the new, and
/// whether the page is cascaded again.
fn beside(held: Held, narrower: f64, cascade: bool, headroom: usize) -> bool {
    let output = held.output as f64 * narrower.clamp(1.0, 64.0);
    let predicted = (output as usize).saturating_add(if cascade { held.styled } else { 0 });
    let total = held
        .tree
        .saturating_add(held.styled)
        .saturating_add(held.output)
        .saturating_add(predicted);
    total <= MAX_PAGE_BYTES && predicted <= headroom
}

/// A page laid out again for `again.want`. With the old layout kept, a
/// refusal returns it (the window keeps showing it); with it let go
/// first, the page steps down from rung 2.
pub fn again(again: Again, measure: &mut Measure, cancel: &AtomicBool) -> Result<Page, Refusal> {
    let Again {
        tree,
        markup,
        rung,
        mut notes,
        budget,
        old,
        keep,
        want,
    } = again;
    let page = &budget;
    let (old, kept, retained) = match old {
        Some(old) if keep => {
            let styled = old.styled().bytes();
            let output = old.bytes();
            (Some(old), styled, output)
        }
        old => {
            drop(old);
            (None, 0, 0)
        }
    };
    let was = old.as_ref().map(|old| old.want);
    let made = (|| {
        let input = markup.as_ref().map_or(0, |markup| markup.len());
        let source = tree.source();
        // A text page counts its text again when its face or scale moved.
        let (tree, shown) = match source {
            Source::Text
                if was.is_none_or(|was| was.scale != want.scale || was.body != want.body) =>
            {
                fitted(tree.try_clone()?, notes.shown, want, measure, cancel, page)?
            }
            _ => (tree.try_clone()?, notes.shown),
        };
        let input = if source == Source::Text {
            node_text(&tree).len() + TEXT_PREFIX.len()
        } else {
            input
        };
        let reuse = old
            .as_ref()
            .filter(|old| redo(old.want, want) == Redo::Layout && shown == notes.shown)
            .map(|old| old.styled_ref().try_clone())
            .transpose()?;
        let styled = match reuse {
            Some(styled) => styled,
            None => {
                let own = want.styles && rung == Rung::Own && source == Source::Markup;
                style(&tree, own, want, input, kept, cancel, page)?
            }
        };
        let own = styled.own;
        let laid = lay(styled, measure, want, retained, cancel, page)?;
        Ok::<_, Refusal>((tree, laid, shown, own))
    })();
    drop(old);
    match made {
        Ok((tree, laid, shown, own)) => {
            notes.shown = shown;
            if own {
                notes.omitted = omitted(&laid).count();
            }
            Ok(Page {
                tree,
                markup,
                laid: Some(laid),
                rung,
                notes,
                budget,
                #[cfg(test)]
                recorder: None,
            })
        }
        Err(Refusal::Cancelled) => Err(Refusal::Cancelled),
        Err(refusal) => {
            stopped(cancel, Step::Down)?;
            if keep {
                return Err(refusal);
            }
            let markup_source = tree.source() == Source::Markup;
            match (rung, markup) {
                (Rung::Own, markup) if markup_source => {
                    if let Refusal::Limit(name) = refusal {
                        notes.styles = Some(name);
                    }
                    user_agent(tree, markup, notes, want, measure, cancel, page)
                }
                (Rung::UserAgent, Some(markup)) => {
                    drop(tree);
                    extracted(markup, refusal, want, measure, cancel, page)
                }
                _ => Err(refusal),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(crate) mod tests {
    use super::*;
    use crate::page::tests::WANT;
    use std::cell::{Cell, RefCell};
    use std::sync::OnceLock;
    use td_ui::prose::Metrics;
    use td_ui::raster::Scale;

    /// Whether a hook sets the flag as a step's check reads it or once the
    /// check has passed, so that the next step's own reads see it first.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) enum When {
        At,
        After,
    }

    /// Where the flag is set: a step's check, when, and past how many of
    /// its earlier ones.
    type Hook = (Step, When, usize);

    thread_local! {
        /// The flag is set at this step's check, past this many of its
        /// earlier ones.
        static CANCEL: Cell<Option<Hook>> = const { Cell::new(None) };
        /// The steps whose check passed, in order.
        static PASSED: RefCell<Vec<Step>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn reach(cancel: &AtomicBool, step: Step, when: When) {
        if when == When::After {
            PASSED.with(|passed| passed.borrow_mut().push(step));
        }
        CANCEL.with(|hook| match hook.get() {
            Some((at, then, 0)) if at == step && then == when => {
                cancel.store(true, Ordering::SeqCst);
                hook.set(None);
            }
            Some((at, then, skip)) if at == step && then == when => {
                hook.set(Some((at, then, skip - 1)));
            }
            _ => {}
        });
    }

    /// `run` with the flag set at `step`'s check, past `skip` of them; the
    /// hook cleared after, whether it fired or not.
    fn cancelling<T>(step: Step, when: When, skip: usize, run: impl FnOnce() -> T) -> T {
        CANCEL.with(|hook| hook.set(Some((step, when, skip))));
        let out = run();
        CANCEL.with(|hook| hook.set(None));
        out
    }

    /// `run`, and the steps whose checks it passed.
    fn passing<T>(run: impl FnOnce() -> T) -> (T, Vec<Step>) {
        PASSED.with(|passed| passed.borrow_mut().clear());
        let out = run();
        (out, PASSED.with(|passed| passed.take()))
    }

    pub(crate) fn measure(want: Want) -> Measure {
        Measure::new(
            Metrics::new(None, Scale::new(want.scale).unwrap()),
            want.body,
        )
    }

    fn markup(page: &Budget, text: &str) -> BudgetShared<BudgetString> {
        BudgetShared::new(page, BudgetString::copy(page, text).unwrap()).unwrap()
    }

    fn made(text: &str, want: Want, page: &Budget) -> Result<Page, Refusal> {
        let cancel = AtomicBool::new(false);
        markup_page(markup(page, text), want, &mut measure(want), &cancel, page)
    }

    #[test]
    fn a_page_is_laid_with_its_own_sheets() {
        let page = Budget::new(256 << 20).unwrap();
        let made = made("<style>p{margin:0}</style><p>a", WANT, &page).unwrap();
        assert_eq!(made.rung, Rung::Own);
        assert_eq!(made.notes, Notes::default());
        let laid = made.laid.unwrap();
        assert!(laid.styled().own);
    }

    #[test]
    fn a_refused_cascade_steps_down_to_the_user_agent_sheets() {
        let page = Budget::new(256 << 20).unwrap();
        // td-css refuses a set past its compiled rules; the page's
        // selectors are its.
        let mut sheet = String::new();
        for group in 0..3 {
            sheet.push_str("<style>");
            for n in 0..7000 {
                sheet.push_str(&format!(".c{group}x{n}{{color:red}}"));
            }
            sheet.push_str("</style>");
        }
        sheet.push_str("<p>text");
        let made = made(&sheet, WANT, &page).unwrap();
        assert_eq!(made.rung, Rung::UserAgent, "{:?}", made.notes);
        assert!(made.notes.styles.is_some());
    }

    #[test]
    fn a_refused_tree_is_laid_as_text() {
        let page = Budget::new(256 << 20).unwrap();
        let deep = format!("{}<p>deep text", "<div>".repeat(300));
        let made = made(&deep, WANT, &page).unwrap();
        assert_eq!(made.rung, Rung::Text);
        assert_eq!(made.notes.text, Some("MAX_DEPTH"));
        assert!(node_text(&made.tree).contains("deep text"));
    }

    #[test]
    fn a_cancelled_job_gives_nothing_and_returns_its_charges() {
        let page = Budget::new(256 << 20).unwrap();
        let used = page.used();
        let cancel = AtomicBool::new(true);
        let made = markup_page(
            markup(&page, "<p>x"),
            WANT,
            &mut measure(WANT),
            &cancel,
            &page,
        );
        assert_eq!(made.err(), Some(Refusal::Cancelled));
        assert_eq!(page.used(), used);
    }

    #[test]
    fn a_text_page_is_cut_by_the_lines_it_lays_at_the_least_width() {
        let page = Budget::new(512 << 20).unwrap();
        // 16 cells a line at 128 px: each 20-letter source line lays two.
        let line = "abcdefghijklmnopqrst\n";
        let text = line.repeat(MAX_TEXT_LINES / 2 + 10);
        let cancel = AtomicBool::new(false);
        let made = text_page(&text, WANT, &mut measure(WANT), &cancel, &page).unwrap();
        let shown = made.notes.shown.unwrap();
        assert_eq!(shown.of, MAX_TEXT_LINES / 2 + 10);
        assert!(shown.lines <= MAX_TEXT_LINES / 2, "{shown:?}");
        assert!(made.laid.unwrap().layout.line_count <= MAX_TEXT_LINES);
    }

    #[test]
    fn a_relayout_reuses_the_styles_for_a_height_or_a_face() {
        let page = Budget::new(256 << 20).unwrap();
        let made = made("<p>words to lay", WANT, &page).unwrap();
        let old = made.laid.unwrap();
        let cancel = AtomicBool::new(false);
        let taller = Want {
            height: 900.0,
            ..WANT
        };
        assert_eq!(redo(WANT, taller), Redo::Layout);
        assert_eq!(
            redo(
                WANT,
                Want {
                    width: 99.0,
                    ..WANT
                }
            ),
            Redo::Cascade
        );
        assert!(fits_beside(&old, taller, usize::MAX));
        assert!(!fits_beside(&old, taller, 0));
        let again = again(
            Again {
                tree: made.tree,
                markup: made.markup,
                rung: made.rung,
                notes: made.notes,
                budget: made.budget,
                old: Some(old.try_clone().unwrap()),
                keep: true,
                want: taller,
            },
            &mut measure(taller),
            &cancel,
        )
        .unwrap();
        let new = again.laid.unwrap();
        assert!(BudgetShared::ptr_eq(new.styled_ref(), old.styled_ref()));
        assert_eq!(new.want, taller);
    }

    fn rank(made: &Result<Page, Refusal>) -> u8 {
        match made {
            Err(_) => 0,
            Ok(page) => match page.rung {
                Rung::Text => 1,
                Rung::UserAgent => 2,
                Rung::Own => 3,
            },
        }
    }

    /// The least of `lo..=hi` at which `ok` holds, `ok` failing at `lo`.
    fn least(mut lo: usize, mut hi: usize, mut ok: impl FnMut(usize) -> bool) -> usize {
        assert!(!ok(lo), "{lo}");
        assert!(ok(hi), "{hi}");
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if ok(mid) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        hi
    }

    /// `text` made in a child of a root holding its markup, limited to
    /// `limit` bytes.
    fn made_within(text: &str, limit: usize) -> (Result<Page, Refusal>, Budget) {
        let root = Budget::new(512 << 20).unwrap();
        let markup = markup(&root, text);
        let page = root.child(limit).unwrap();
        let cancel = AtomicBool::new(false);
        let made = markup_page(markup, WANT, &mut measure(WANT), &cancel, &page);
        (made, page)
    }

    /// A page each rung of whose ladder a lowered limit reaches: its own
    /// sheet costs more to compile than the user-agent sheets alone, and
    /// its elements more to parse and cascade than its text to lay.
    fn ladder() -> String {
        let mut text = String::from("<style>");
        for n in 0..2000 {
            text.push_str(&format!(".c{n}{{color:red}}"));
        }
        text.push_str("</style><p>some text");
        text.push_str(&"<i></i>".repeat(2000));
        text
    }

    /// The least page limits at which `ladder()` is laid as text, with the
    /// user-agent sheets and with its own (`rank` 1, 2 and 3).
    fn ladder_limits() -> [usize; 3] {
        static LIMITS: OnceLock<[usize; 3]> = OnceLock::new();
        *LIMITS.get_or_init(|| {
            let text = ladder();
            let mut limits = [0; 3];
            for (at, r) in limits.iter_mut().zip(1u8..) {
                *at = least(2 * Budget::metadata_bytes(), 64 << 20, |limit| {
                    rank(&made_within(&text, limit).0) >= r
                });
            }
            limits
        })
    }

    #[test]
    fn each_rung_is_reached_by_a_lowered_limit() {
        let text = ladder();
        let [as_text, user_agent, own] = ladder_limits();
        let empty = Budget::metadata_bytes();
        // Rung 4: nothing fits, not even the text.
        let (made, page) = made_within(&text, as_text - 1);
        assert!(matches!(made, Err(Refusal::Limit(_))), "{:?}", made.err());
        assert_eq!(page.used(), empty);
        // Rung 3, the tree refused.
        let (made, page) = made_within(&text, as_text);
        let page_made = made.unwrap();
        assert_eq!(page_made.rung, Rung::Text);
        assert_eq!(page_made.notes.text, Some("MAX_TREE_BYTES"));
        assert!(node_text(&page_made.tree).contains("some text"));
        drop(page_made);
        assert_eq!(page.used(), empty);
        // Rung 3, rung 2 refused: the tree parsed, both cascades not.
        let (made, page) = made_within(&text, user_agent - 1);
        let page_made = made.unwrap();
        assert_eq!(page_made.rung, Rung::Text);
        assert_eq!(page_made.notes.text, Some("memory"));
        drop(page_made);
        assert_eq!(page.used(), empty);
        // Rung 2: its own sheet refused, the user-agent sheets alone laid.
        let (made, page) = made_within(&text, user_agent);
        let page_made = made.unwrap();
        assert_eq!(page_made.rung, Rung::UserAgent);
        assert_eq!(page_made.notes.styles, Some("memory"));
        assert!(!page_made.laid.as_ref().unwrap().styled().own);
        drop(page_made);
        assert_eq!(page.used(), empty);
        // Rung 1. Each step's charge is shrunk to what the page keeps once
        // the step is done: its own sheet's compiled set, which alone
        // parts rung 1 from rung 2, and construction's index and scratch
        // are returned, and the page holds less than rung 2 needed.
        let (made, page) = made_within(&text, own);
        let page_made = made.unwrap();
        assert_eq!(page_made.rung, Rung::Own);
        assert_eq!(page_made.notes, Notes::default());
        assert!(page.used() < user_agent, "{} {user_agent}", page.used());
        drop(page_made);
        assert_eq!(page.used(), empty);
    }

    /// `text` made with `n` reservations admitted: its refusal, the steps
    /// of its first rung whose checks passed, and whether its page's
    /// charges were all returned once it was let go.
    fn reserved(text: &str, n: usize) -> (Option<Refusal>, Vec<Step>, bool) {
        let root = Budget::new(64 << 20).unwrap();
        let markup = markup(&root, text);
        let page = root.child(64 << 20).unwrap();
        let used = page.used();
        let mut measure = measure(WANT);
        let cancel = AtomicBool::new(false);
        let (made, mut passed) = passing(|| {
            td_alloc::with_reservation_limit(n, || {
                markup_page(markup, WANT, &mut measure, &cancel, &page)
            })
        });
        let refused = made.err();
        let first = passed.iter().position(|&step| step == Step::Down);
        passed.truncate(first.unwrap_or(passed.len()));
        (refused, passed, page.used() == used)
    }

    #[test]
    fn each_steps_charge_refused_is_returned() {
        let text = "<style>p{margin:0}</style><p>a";
        let steps = [Step::Parse, Step::Compile, Step::Cascade, Step::Layout];
        let total = least(0, 1 << 20, |n| reserved(text, n).0.is_none());
        for (at, step) in steps.iter().enumerate() {
            // The least admission past which the step's check is reached:
            // one fewer refuses the last reservation of the step's own.
            let n = least(0, total, |n| reserved(text, n).1.contains(step));
            let (refused, passed, returned) = reserved(text, n - 1);
            assert_eq!(refused, Some(Refusal::Limit("memory")), "{step:?}");
            assert_eq!(passed, steps[..at], "{step:?}");
            assert!(returned, "{step:?}");
        }
        // Past the last check: the page's own handles.
        let (refused, passed, returned) = reserved(text, total - 1);
        assert_eq!(refused, Some(Refusal::Limit("memory")));
        assert_eq!(passed, steps);
        assert!(returned);
    }

    /// `text` made within `limit` with the flag set at a step's check, as
    /// `cancelling` says: the refusal, the steps whose checks passed, and
    /// whether the page's charges were all returned.
    fn cancelled(
        text: &str,
        limit: usize,
        (step, when, skip): Hook,
    ) -> (Option<Refusal>, Vec<Step>, bool) {
        let root = Budget::new(512 << 20).unwrap();
        let markup = markup(&root, text);
        let page = root.child(limit).unwrap();
        let used = page.used();
        let mut measure = measure(WANT);
        let cancel = AtomicBool::new(false);
        let (made, passed) = passing(|| {
            cancelling(step, when, skip, || {
                markup_page(markup, WANT, &mut measure, &cancel, &page)
            })
        });
        assert!(cancel.load(Ordering::SeqCst), "{step:?} {when:?} {skip}");
        let refused = made.err();
        (refused, passed, page.used() == used)
    }

    #[test]
    fn a_job_cancelled_at_each_step_gives_cancelled_and_returns_its_charges() {
        use Step::*;
        use When::*;
        let text = "<style>p{margin:0}</style><p>a";
        let cases: &[(Hook, &[Step])] = &[
            ((Parse, At, 0), &[]),
            ((Compile, At, 0), &[Parse]),
            ((Cascade, At, 0), &[Parse, Compile]),
            // After layout, before its result is taken.
            ((Layout, At, 0), &[Parse, Compile, Cascade]),
            // td-layout's own reads, as it cascades and as it lays out.
            ((Compile, After, 0), &[Parse, Compile]),
            ((Cascade, After, 0), &[Parse, Compile, Cascade]),
        ];
        for &(hook, steps) in cases {
            let (refused, passed, returned) = cancelled(text, 64 << 20, hook);
            assert_eq!(refused, Some(Refusal::Cancelled), "{hook:?}");
            assert_eq!(passed, steps, "{hook:?}");
            assert!(returned, "{hook:?}");
        }
        // Rung 3's extraction.
        let deep = format!("{}<p>deep text", "<div>".repeat(300));
        let (refused, passed, returned) = cancelled(&deep, 64 << 20, (Extract, At, 0));
        assert_eq!(refused, Some(Refusal::Cancelled));
        assert_eq!(passed, [Down]);
        assert!(returned);
        // A text page's parse and count.
        for (hook, steps) in [
            ((Parse, At, 0), &[][..]),
            ((Count, At, 0), &[Parse, Compile, Cascade]),
        ] {
            let page = Budget::new(64 << 20).unwrap();
            let used = page.used();
            let cancel = AtomicBool::new(false);
            let (made, passed) = passing(|| {
                cancelling(hook.0, hook.1, hook.2, || {
                    text_page("a\tb", WANT, &mut measure(WANT), &cancel, &page)
                })
            });
            assert_eq!(made.err(), Some(Refusal::Cancelled), "{hook:?}");
            assert_eq!(passed, steps, "{hook:?}");
            assert_eq!(page.used(), used);
        }
    }

    #[test]
    fn a_job_cancelled_between_rungs_does_not_step_down() {
        use Step::*;
        use When::*;
        // The tree refused: no extraction.
        let deep = format!("{}<p>deep text", "<div>".repeat(300));
        let (refused, passed, returned) = cancelled(&deep, 64 << 20, (Down, At, 0));
        assert_eq!(refused, Some(Refusal::Cancelled));
        assert!(passed.is_empty(), "{passed:?}");
        assert!(returned);
        // Within this limit its own sheet is refused, then the user-agent
        // sheets, and the page is laid as text (above): cancelled as rung 1
        // is refused, and as rung 2 is.
        let text = ladder();
        let limit = ladder_limits()[1] - 1;
        for skip in [0, 1] {
            let (refused, passed, returned) = cancelled(&text, limit, (Down, At, skip));
            assert_eq!(refused, Some(Refusal::Cancelled), "{skip}");
            assert_eq!(passed.iter().filter(|&&step| step == Down).count(), skip);
            assert!(passed.contains(&Parse) && !passed.contains(&Extract));
            assert!(returned, "{skip}");
        }
    }

    #[test]
    fn td_layouts_own_reads_of_the_flag_give_cancelled() {
        assert_eq!(Refusal::from(Error::Cancelled), Refusal::Cancelled);
        assert_eq!(
            Refusal::from(Error::Limit("MAX_NODES")),
            Refusal::Limit("MAX_NODES")
        );
        let page = Budget::new(64 << 20).unwrap();
        let tree = Tree::parse("<p>a", Source::Markup, &page).unwrap();
        let tree = BudgetShared::new(&page, tree).unwrap();
        let used = page.used();
        // The cascade reads the flag set once the compile's check passed;
        // its refusal is taken as cancelled, not as a limit to step down
        // from.
        let cancel = AtomicBool::new(false);
        let styled = cancelling(Step::Compile, When::After, 0, || {
            style(&tree, true, WANT, 4, 0, &cancel, &page)
        });
        assert_eq!(styled.err(), Some(Refusal::Cancelled));
        assert_eq!(page.used(), used);
        let cancel = AtomicBool::new(false);
        let styled = style(&tree, true, WANT, 4, 0, &cancel, &page).unwrap();
        cancel.store(true, Ordering::SeqCst);
        let laid = lay(styled, &mut measure(WANT), WANT, 0, &cancel, &page);
        assert_eq!(laid.err(), Some(Refusal::Cancelled));
        assert_eq!(page.used(), used);
    }

    /// `made`'s parts for a relayout at `want` on `rung`, in `budget`; the
    /// old layout given with it, kept or to be let go.
    fn again_of(
        made: &Page,
        rung: Rung,
        old: Option<BudgetShared<Laid>>,
        keep: bool,
        want: Want,
        budget: &Budget,
    ) -> Again {
        Again {
            tree: made.tree.try_clone().unwrap(),
            markup: made.markup.as_ref().map(|m| m.try_clone().unwrap()),
            rung,
            notes: made.notes,
            budget: budget.try_clone().unwrap(),
            old,
            keep,
            want,
        }
    }

    fn relaid(again: Again) -> Result<Page, Refusal> {
        let want = again.want;
        let cancel = AtomicBool::new(false);
        super::again(again, &mut measure(want), &cancel)
    }

    const WIDER: Want = Want {
        width: 600.0,
        ..WANT
    };

    #[test]
    fn a_refused_relayout_kept_beside_keeps_the_old_layout() {
        let root = Budget::new(512 << 20).unwrap();
        let made = made(&ladder(), WANT, &root).unwrap();
        assert_eq!(made.rung, Rung::Own);
        let old = made.laid.as_ref().unwrap();
        // A lowered limit for the relayout's own charges at which its own
        // sheet is refused and the user-agent sheets alone fit: let go
        // first, the page steps down to rung 2 there.
        let lo = 2 * Budget::metadata_bytes();
        let limit = least(lo, 64 << 20, |limit| {
            let child = root.child(limit).unwrap();
            rank(&relaid(again_of(
                &made,
                Rung::Own,
                None,
                false,
                WIDER,
                &child,
            ))) >= 2
        });
        let child = root.child(limit).unwrap();
        let down = relaid(again_of(&made, Rung::Own, None, false, WIDER, &child)).unwrap();
        assert_eq!(down.rung, Rung::UserAgent);
        assert_eq!(down.notes.styles, Some("memory"));
        drop(down);
        // Kept beside: refused, with no step down, the old layout intact.
        let used = root.used();
        let child = root.child(limit).unwrap();
        let kept = Some(old.try_clone().unwrap());
        let refused = relaid(again_of(&made, Rung::Own, kept, true, WIDER, &child));
        assert_eq!(refused.err(), Some(Refusal::Limit("memory")));
        drop(child);
        assert_eq!(root.used(), used);
        assert_eq!(old.want, WANT);
        assert!(old.styled().own);
        assert!(old.layout.line_count > 0);
        // Cancelled as it was refused.
        let child = root.child(limit).unwrap();
        let kept = Some(old.try_clone().unwrap());
        let cancel = AtomicBool::new(false);
        let refused = cancelling(Step::Down, When::At, 0, || {
            again(
                again_of(&made, Rung::Own, kept, true, WIDER, &child),
                &mut measure(WIDER),
                &cancel,
            )
        });
        assert_eq!(refused.err(), Some(Refusal::Cancelled));
    }

    #[test]
    fn a_let_go_relayout_steps_down_from_the_user_agent_sheets_to_text() {
        let (made, _) = made_within(&ladder(), ladder_limits()[1]);
        let mut made = made.unwrap();
        assert_eq!(made.rung, Rung::UserAgent);
        // The window gives up the layout with the request.
        drop(made.laid.take());
        let page = made.budget.try_clone().unwrap();
        let lo = 2 * Budget::metadata_bytes();
        let limit = least(lo, page.limit(), |limit| {
            let child = page.child(limit).unwrap();
            rank(&relaid(again_of(
                &made, made.rung, None, false, WIDER, &child,
            ))) >= 1
        });
        let child = page.child(limit).unwrap();
        let text = relaid(again_of(&made, made.rung, None, false, WIDER, &child)).unwrap();
        assert_eq!(text.rung, Rung::Text);
        assert_eq!(text.notes.text, Some("memory"));
        assert!(node_text(&text.tree).contains("some text"));
        // Cancelled as rung 2 is refused: no text.
        let child = page.child(limit).unwrap();
        let cancel = AtomicBool::new(false);
        let refused = cancelling(Step::Down, When::At, 0, || {
            again(
                again_of(&made, made.rung, None, false, WIDER, &child),
                &mut measure(WIDER),
                &cancel,
            )
        });
        assert_eq!(refused.err(), Some(Refusal::Cancelled));
    }

    #[test]
    fn a_refused_relayout_of_a_text_page_has_no_rung_below() {
        let root = Budget::new(64 << 20).unwrap();
        let cancel = AtomicBool::new(false);
        let made = text_page("some text", WANT, &mut measure(WANT), &cancel, &root).unwrap();
        let child = root.child(2 * Budget::metadata_bytes()).unwrap();
        let refused = relaid(again_of(&made, made.rung, None, false, WIDER, &child));
        assert_eq!(refused.err(), Some(Refusal::Limit("memory")));
    }

    #[test]
    fn a_relayout_that_does_not_fit_beside_lets_the_old_layout_go_first() {
        // Many lines and few elements: the output outweighs the cascade.
        let text = format!("<p>{}", "word ".repeat(20_000));
        let lo = 2 * Budget::metadata_bytes();
        let limit = least(lo, 64 << 20, |limit| {
            rank(&made_within(&text, limit).0) >= 3
        });
        let (made, page) = made_within(&text, limit);
        let mut made = made.unwrap();
        let taller = Want {
            height: 900.0,
            ..WANT
        };
        let headroom = page.limit() - page.used();
        let old = made.laid.take().unwrap();
        assert!(!fits_beside(&old, taller, headroom));
        // Kept beside, as a mutant forcing it would: refused.
        let kept = Some(old.try_clone().unwrap());
        let refused = relaid(again_of(&made, made.rung, kept, true, taller, &page));
        assert_eq!(refused.err(), Some(Refusal::Limit("memory")));
        // Let go first: laid.
        let again = relaid(again_of(&made, made.rung, Some(old), false, taller, &page)).unwrap();
        assert_eq!(again.rung, Rung::Own);
        assert_eq!(again.laid.unwrap().want, taller);
    }

    #[test]
    fn the_prediction_reads_the_width_the_cascade_and_the_headroom() {
        let held = Held {
            tree: 10,
            styled: 20,
            output: 30,
        };
        assert!(beside(held, 1.0, false, 30));
        assert!(!beside(held, 1.0, false, 29));
        // Narrower, the output grows with the ratio; wider, it does not
        // shrink; at most 64 times.
        assert!(beside(held, 2.0, false, 60));
        assert!(!beside(held, 2.0, false, 59));
        assert!(!beside(held, 0.5, false, 29));
        assert!(beside(held, 1000.0, false, 30 * 64));
        assert!(!beside(held, 1000.0, false, 30 * 64 - 1));
        // Cascading again predicts a styled document too.
        assert!(beside(held, 1.0, true, 50));
        assert!(!beside(held, 1.0, true, 49));
        // Within the page term: the tree, the old styled document and
        // output and the prediction.
        let full = Held {
            tree: MAX_PAGE_BYTES - 80,
            ..held
        };
        assert!(beside(full, 1.0, false, usize::MAX));
        let past = Held {
            tree: MAX_PAGE_BYTES - 79,
            ..held
        };
        assert!(!beside(past, 1.0, false, usize::MAX));
    }

    /// The cutover's host measurement (DESIGN.md §10, increment 7): each
    /// `.html` file under `$TD_WEB_CORPUS` (fetched into scratch, never
    /// committed) and synthetic shapes made, styled and laid on the cell,
    /// then laid again at another width and painted whole; times and the
    /// page budget's peak printed.
    #[test]
    #[ignore = "host measurement: TD_WEB_CORPUS, release build"]
    fn measure_pages() {
        use std::time::Instant;
        let mut pages: Vec<(String, String)> = Vec::new();
        if let Some(dir) = std::env::var_os("TD_WEB_CORPUS") {
            let mut names: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|e| e == "html"))
                .collect();
            names.sort();
            for path in names {
                let bytes = std::fs::read(&path).unwrap();
                let text = String::from_utf8_lossy(&bytes);
                let cut = text.floor_char_boundary(crate::document::MAX_MARKUP);
                pages.push((path.display().to_string(), text[..cut].to_string()));
            }
        }
        let paragraphs = "<p>abcdefghijklmnop".repeat((2 << 20) / 19);
        pages.push(("2 MiB of sixteen-letter paragraphs".into(), paragraphs));
        let mut hostile = String::from("<style>");
        for n in 0..5000 {
            hostile.push_str(&format!(":not(.c{n}) {{ margin: 0 }}"));
        }
        hostile.push_str("</style>");
        hostile.push_str(&"<p>text".repeat(5000));
        pages.push((
            "5,000 universal :not() rules, 5,000 paragraphs".into(),
            hostile,
        ));
        for (name, text) in pages {
            let page = Budget::new(memory::PAGES).unwrap();
            let cancel = AtomicBool::new(false);
            let started = Instant::now();
            let made = markup_page(
                markup(&page, &text),
                WANT,
                &mut measure(WANT),
                &cancel,
                &page,
            );
            let first = started.elapsed();
            let Ok(made) = made else {
                println!("{name}: refused {:?}", made.err());
                continue;
            };
            if let Some(limit) = made.notes.styles.or(made.notes.text) {
                println!("{name}: stepped down by {limit}");
            }
            let peak = page.used();
            let wider = Want {
                width: 1200.0,
                ..WANT
            };
            let old = made.laid.as_ref().unwrap().try_clone().unwrap();
            let started = Instant::now();
            let again = again(
                Again {
                    tree: made.tree.try_clone().unwrap(),
                    markup: made.markup.as_ref().map(|m| m.try_clone().unwrap()),
                    rung: made.rung,
                    notes: made.notes,
                    budget: page.try_clone().unwrap(),
                    old: Some(old),
                    keep: true,
                    want: wider,
                },
                &mut measure(wider),
                &cancel,
            );
            let relaid = started.elapsed();
            let laid = made.laid.unwrap();
            let memory = Budget::new(64 << 20).unwrap();
            let mut scratch = crate::paint::Scratch::new(&memory);
            let pane = td_ui::raster::Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1200,
            };
            let frame = crate::paint::Frame {
                pane,
                x: 0,
                y: 0,
                scale: 1,
            };
            let started = Instant::now();
            let mut draws = 0usize;
            let painted = crate::paint::paint(
                &laid,
                &mut measure(WANT),
                &frame,
                pane,
                &crate::paint::Marks::default(),
                &mut scratch,
                &mut |_| draws += 1,
            );
            let paint = started.elapsed();
            println!(
                "{name}: rung {:?}, {} lines, made {first:?}, relaid {relaid:?} ({}), \
                 painted {paint:?} ({draws} draws, cut {}), page peak {:.1} MiB, \
                 page_bytes {:.1} MiB",
                made.rung,
                laid.layout.line_count,
                if again.is_ok() { "ok" } else { "refused" },
                painted.cut,
                peak as f64 / 1048576.0,
                laid.layout.page_bytes as f64 / 1048576.0,
            );
        }
    }
}
