//! Scalar wrapping with DOM-byte provenance. No retained glyph array.
use crate::list::{MarkerFragment, MarkerKind};
use crate::{
    cancelled, keyword, Error, Limits, StyleId, StyledDocument, CANCEL_INTERVAL, MAX_COORDINATE,
};
use std::mem::size_of;
use std::ops::Range;
use std::sync::atomic::AtomicBool;
use td_alloc::{Budget, BudgetVec};
use td_css::values::Length;
use td_css::{ComputedStyle, ComputedValue, Property};
use td_html::document::NodeId;

pub const UNIT: u32 = 64;
/// Where white space is preserved, a tab advances to the next multiple of
/// this many spaces' advance from the line's start (CSS tab-size: 8).
pub const TAB_SPACES: i64 = 8;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FontLine {
    pub height: u32,
    pub baseline: u32,
}
/// Units are 1/64 CSS px. Providers own and bound their caches.
pub trait FontMetrics {
    fn advance(&mut self, style: &ComputedStyle, scalar: char) -> u32;
    fn line(&mut self, style: &ComputedStyle) -> FontLine;
}
#[derive(Clone, Debug)]
pub struct TextRun {
    pub node: NodeId,
    pub range: Range<usize>,
    pub style: StyleId,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayText {
    Source,
    Space,
    Replacement,
}
#[derive(Clone, Debug, PartialEq)]
pub struct TextFragment {
    pub node: NodeId,
    pub range: Range<usize>,
    pub style: StyleId,
    pub display: DisplayText,
    pub x: f64,
    pub width: f64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ReplacedFragment {
    pub node: NodeId,
    pub style: StyleId,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
/// A single fragment lives in its line; longer runs own budgeted storage.
pub struct Fragments<T>(FragmentStorage<T>);
#[derive(Debug)]
enum FragmentStorage<T> {
    One(Option<T>),
    Many(BudgetVec<T>),
}
impl<T> Fragments<T> {
    fn new() -> Self {
        Self(FragmentStorage::One(None))
    }
    fn add(&mut self, value: T, memory: &Budget, counters: &mut Counters) -> Result<(), Error> {
        match &mut self.0 {
            FragmentStorage::One(first) if first.is_none() => *first = Some(value),
            FragmentStorage::One(first) => {
                let mut values = BudgetVec::new(memory)?;
                // Reserve both before moving the first fragment out.
                counters.charge(
                    2usize
                        .checked_mul(size_of::<T>())
                        .and_then(|n| n.checked_mul(2))
                        .and_then(|n| n.checked_add(32))
                        .ok_or(Error::Limit("MAX_PAGE_BYTES"))?,
                )?;
                BudgetVec::try_reserve_exact(&mut values, 2)?;
                values.try_push(first.take().ok_or(Error::Invalid)?)?;
                values.try_push(value)?;
                self.0 = FragmentStorage::Many(values);
            }
            FragmentStorage::Many(values) => {
                counters.admit_growth(values)?;
                values.try_push(value)?;
            }
        }
        Ok(())
    }
}
impl<T> std::ops::Deref for Fragments<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match &self.0 {
            FragmentStorage::One(value) => value.as_slice(),
            FragmentStorage::Many(values) => values,
        }
    }
}
impl<T> std::ops::DerefMut for Fragments<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match &mut self.0 {
            FragmentStorage::One(value) => value.as_mut_slice(),
            FragmentStorage::Many(values) => values,
        }
    }
}
impl<T: std::fmt::Debug> std::fmt::Debug for Fragments<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}
impl<T: PartialEq> PartialEq for Fragments<T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl<'a, T> IntoIterator for &'a Fragments<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl<'a, T> IntoIterator for &'a mut Fragments<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}
#[derive(Debug, PartialEq)]
pub struct Line {
    pub y: f64,
    pub height: f64,
    pub baseline: f64,
    /// Content advance; negative replaced-element margins can make it signed.
    pub width: f64,
    pub fragments: Fragments<TextFragment>,
    pub replaced: Fragments<ReplacedFragment>,
}
pub struct WrappedText {
    pub lines: BudgetVec<Line>,
    pub charged_bytes: usize,
    pub work: usize,
    pub cuts: Cuts,
}
/// Lines that do not wrap, cut at Limits::line_scalars: what follows a
/// cut, to its line's end, is not laid and has no fragment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cuts {
    /// Lines cut, counted whether emitted or not.
    pub lines: usize,
    /// The first item, scalar or replaced element, the first cut line
    /// dropped at the bound. Scalars before it can lack fragments too,
    /// as trailing spaces do.
    pub first: Option<Cut>,
}
/// A text node and a byte offset in it, or a replaced element and 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cut {
    pub node: NodeId,
    pub offset: usize,
}
/// One scalar of a laid text fragment, replayed through the metrics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replayed {
    /// Its bytes in the fragment's text node.
    pub range: Range<usize>,
    /// As shown: ' ' for a collapsed space, U+FFFD for a replaced scalar,
    /// '\t' for a preserved tab, which draws nothing.
    pub shown: char,
    /// From the fragment's x, in 1/64 CSS px.
    pub pen: i64,
    pub advance: i64,
}
#[derive(Clone)]
pub(crate) struct Pending {
    node: NodeId,
    range: Range<usize>,
    style: StyleId,
    display: DisplayText,
    pen: i64,
    advance: i64,
    above: i64,
    below: i64,
    space: bool,
    /// A preserved tab: its advance is its stop interval until queued.
    tab: bool,
    atomic: Option<Atomic>,
}
/// An item laid whole: an image, or an inside list marker.
#[derive(Clone, Copy)]
enum Atomic {
    Image(f64, f64, [f64; 4]),
    Marker(MarkerKind, i32),
}
/// A list item's marker, measured: its item, style, text and line metrics,
/// all in 1/64 CSS px.
#[derive(Clone, Copy)]
pub(crate) struct Marker {
    pub item: NodeId,
    pub style: StyleId,
    pub kind: MarkerKind,
    pub ordinal: i32,
    pub advance: i64,
    pub above: i64,
    pub below: i64,
    /// Its text's scalars, the work measuring it costs.
    pub scalars: usize,
}
impl Marker {
    /// Measures `kind`'s text for `ordinal` in the item's style, a work
    /// unit a scalar (at most MAX_MARKER_BYTES); `known` is the style's
    /// line metrics when already measured.
    pub fn new(
        styled: &StyledDocument<'_>,
        metrics: &mut impl FontMetrics,
        (item, style): (NodeId, StyleId),
        (kind, ordinal): (MarkerKind, i32),
        known: Option<(i64, i64)>,
        counters: &mut Counters,
    ) -> Result<Self, Error> {
        let computed = styled.style(style).ok_or(Error::Invalid)?;
        let spacing = letter_spacing(computed);
        let text = kind.text(ordinal);
        let mut width: i64 = 0;
        let mut scalars = 0;
        for scalar in text.as_str().chars() {
            counters.step(1)?;
            scalars += 1;
            width = width
                .checked_add(advance(metrics, computed, scalar, spacing))
                .ok_or(Error::Invalid)?;
        }
        if !coordinate(width as f64 / 64.0) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        let (above, below) = match known {
            Some(known) => known,
            None => vertical(metrics, computed)?,
        };
        Ok(Self {
            item,
            style,
            kind,
            ordinal,
            advance: width,
            above,
            below,
            scalars,
        })
    }
    /// Placed with its end at `end` and its baseline at `baseline`.
    pub fn fragment(&self, end: f64, baseline: f64, inside: bool) -> MarkerFragment {
        let width = self.advance as f64 / 64.0;
        MarkerFragment {
            item: self.item,
            ordinal: self.ordinal,
            kind: self.kind,
            inside,
            style: self.style,
            line: None,
            x: end - width,
            y: baseline - self.above as f64 / 64.0,
            width,
            height: (self.above + self.below).max(0) as f64 / 64.0,
            baseline,
        }
    }
}
pub(crate) struct Counters<'c> {
    pub bytes: usize,
    pub work: usize,
    pub lines: usize,
    pub fragments: usize,
    pub cuts: Cuts,
    /// All-space line starts dropped before a word that moved (§4): a
    /// wider line could keep each, with its word, as a line of its own.
    pub indents: usize,
    pub limits: Limits<'static>,
    cancel: Option<&'c AtomicBool>,
    next_check: usize,
}
impl<'c> Counters<'c> {
    pub fn new(styled: &StyledDocument<'_>, cancel: Option<&'c AtomicBool>) -> Self {
        Self {
            bytes: styled.charged_bytes(),
            work: 0,
            lines: 0,
            fragments: 0,
            cuts: Cuts::default(),
            indents: 0,
            limits: styled.limits(),
            cancel,
            next_check: 0,
        }
    }
    /// Every visit ticks here, so the flag is read once per CANCEL_INTERVAL
    /// units and at the first.
    pub fn step(&mut self, count: usize) -> Result<(), Error> {
        self.work = self
            .work
            .checked_add(count)
            .filter(|n| *n <= self.limits.work)
            .ok_or(Error::Limit("MAX_LAYOUT_WORK"))?;
        if self.work >= self.next_check {
            self.next_check = self.work.saturating_add(CANCEL_INTERVAL);
            if cancelled(self.cancel) {
                return Err(Error::Cancelled);
            }
        }
        Ok(())
    }
    pub fn admit_growth<T>(&mut self, values: &mut BudgetVec<T>) -> Result<(), Error> {
        if values.len() == values.capacity() {
            let capacity = values
                .capacity()
                .checked_mul(2)
                .map(|n| n.max(1))
                .ok_or(Error::Limit("MAX_PAGE_BYTES"))?;
            let added = capacity - values.capacity();
            let charge = added
                .checked_mul(size_of::<T>())
                .and_then(|n| n.checked_mul(2))
                .and_then(|n| n.checked_add(32))
                .ok_or(Error::Limit("MAX_PAGE_BYTES"))?;
            self.charge(charge)?;
            BudgetVec::try_reserve_exact(values, capacity - values.len())?;
        }
        Ok(())
    }
    pub fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|n| *n <= self.limits.bytes)
            .ok_or(Error::Limit("MAX_PAGE_BYTES"))?;
        Ok(())
    }
}
pub(crate) struct LineBounds {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub advance: f64,
    /// The line box's own top, height and baseline from its top.
    pub y: f64,
    pub height: f64,
    pub baseline: f64,
    /// The byte its first item starts at, if it has one.
    pub start: Option<usize>,
}
pub(crate) trait LineSink {
    /// Every line, retained or not.
    fn measure(&mut self, _bounds: LineBounds, _counters: &mut Counters) -> Result<(), Error> {
        Ok(())
    }
    fn wants(&self, y: f64, height: f64) -> bool;
    /// A retained line, with the inside marker it starts with, if any.
    fn line(
        &mut self,
        line: Line,
        marker: Option<MarkerFragment>,
        counters: &mut Counters,
    ) -> Result<(), Error>;
}
struct AllLines(BudgetVec<Line>);
impl LineSink for AllLines {
    fn wants(&self, _: f64, _: f64) -> bool {
        true
    }
    fn line(
        &mut self,
        line: Line,
        _: Option<MarkerFragment>,
        counters: &mut Counters,
    ) -> Result<(), Error> {
        counters.admit_growth(&mut self.0)?;
        self.0.try_push(line)?;
        Ok(())
    }
}
/// A finite run list is one inline sequence, with no implicit breaks at runs.
pub fn wrap_text(
    styled: &StyledDocument<'_>,
    metrics: &mut impl FontMetrics,
    runs: &[TextRun],
    width: u32,
    strut: StyleId,
) -> Result<WrappedText, Error> {
    wrap_text_with_budget(styled, metrics, runs, width, strut, styled.budget())
}
pub fn wrap_text_with_budget(
    styled: &StyledDocument<'_>,
    metrics: &mut impl FontMetrics,
    runs: &[TextRun],
    width: u32,
    strut: StyleId,
    memory: &Budget,
) -> Result<WrappedText, Error> {
    let mut counters = Counters::new(styled, None);
    counters.step(runs.len())?;
    let mut sink = AllLines(BudgetVec::new(memory)?);
    let mut pending = BudgetVec::new(memory)?;
    let mut flow = Flow::new(styled, metrics, width, strut, 0.0, memory, &mut pending)?;
    for run in runs {
        flow.text(run, &mut counters, &mut sink)?;
    }
    flow.finish(&mut counters, &mut sink)?;
    Ok(WrappedText {
        lines: sink.0,
        charged_bytes: counters.bytes,
        work: counters.work,
        cuts: counters.cuts,
    })
}
pub(crate) struct ImageRun {
    pub node: NodeId,
    pub style: StyleId,
    pub width: f64,
    pub height: f64,
    pub margin: [f64; 4],
}
pub(crate) struct Flow<'s, 'd, 'm, M> {
    styled: &'s StyledDocument<'d>,
    memory: &'s Budget,
    metrics: &'m mut M,
    width: u32,
    pending: &'m mut BudgetVec<Pending>,
    pen: i64,
    after_space: Option<usize>,
    last_space: bool,
    after_newline: bool,
    previous_cr: bool,
    /// The first place this line dropped at Limits::line_scalars, and
    /// whether it dropped more than spaces, which counts the line cut.
    cut: Option<Cut>,
    cut_counted: bool,
    /// While cut: a wrap opportunity was dropped since the last non-space.
    cut_opportunity: bool,
    /// The block's style, whose space sets the tab interval, found once.
    strut: StyleId,
    stops: Option<i64>,
    above: i64,
    below: i64,
    pub y: f64,
}
impl<'s, 'd, 'm, M: FontMetrics> Flow<'s, 'd, 'm, M> {
    pub fn new(
        styled: &'s StyledDocument<'d>,
        metrics: &'m mut M,
        width: u32,
        strut: StyleId,
        y: f64,
        memory: &'s Budget,
        pending: &'m mut BudgetVec<Pending>,
    ) -> Result<Self, Error> {
        if f64::from(width) > MAX_COORDINATE || !coordinate(y) {
            return Err(Error::Invalid);
        }
        pending.clear();
        let style = styled.style(strut).ok_or(Error::Invalid)?;
        let (above, below) = vertical(metrics, style)?;
        Ok(Self {
            styled,
            memory,
            metrics,
            width,
            pending,
            pen: 0,
            after_space: None,
            last_space: true,
            after_newline: false,
            previous_cr: false,
            cut: None,
            cut_counted: false,
            cut_opportunity: false,
            strut,
            stops: None,
            above,
            below,
            y,
        })
    }
    pub fn text(
        &mut self,
        run: &TextRun,
        counters: &mut Counters,
        sink: &mut impl LineSink,
    ) -> Result<(), Error> {
        let text = self
            .styled
            .document()
            .node(run.node)
            .and_then(|n| n.text())
            .and_then(|s| s.get(run.range.start..run.range.end))
            .ok_or(Error::Invalid)?;
        let style = self.styled.style(run.style).ok_or(Error::Invalid)?;
        let (preserve, newline, soft) = white_space(style);
        let (above, below) = vertical(self.metrics, style)?;
        let spacing = letter_spacing(style);
        for (at, scalar) in text.char_indices() {
            counters.step(1)?;
            if self.previous_cr && scalar == '\n' && newline {
                self.previous_cr = false;
                continue;
            }
            self.previous_cr = scalar == '\r' && newline;
            if newline && matches!(scalar, '\n' | '\r') {
                self.hard_break(run.style, counters, sink)?;
                self.previous_cr = scalar == '\r';
                continue;
            }
            let ascii_space = matches!(scalar, ' ' | '\t' | '\n' | '\r' | '\x0c');
            if !preserve && ascii_space && self.last_space {
                continue;
            }
            let (shown, display) = shown(scalar, preserve);
            self.last_space = !preserve && ascii_space;
            let space = matches!(shown, ' ' | '\t');
            let offset = run.range.start + at;
            // A cut line drops it unmeasured; its work is charged above.
            if self.cut.is_some() && self.drops(space, soft) {
                self.drop_item(run.node, offset, space, soft, counters)?;
                continue;
            }
            let (advance, tab) = if shown == '\t' {
                // The interval: queue lays it from the pen it then has.
                (self.stops()?, true)
            } else {
                (advance(self.metrics, style, shown, spacing), false)
            };
            let item = Pending {
                node: run.node,
                range: offset..offset + scalar.len_utf8(),
                style: run.style,
                display,
                pen: 0,
                advance,
                above,
                below,
                space,
                tab,
                atomic: None,
            };
            self.queue(item, soft, counters, sink)?;
        }
        Ok(())
    }
    pub fn image(
        &mut self,
        image: ImageRun,
        counters: &mut Counters,
        sink: &mut impl LineSink,
    ) -> Result<(), Error> {
        let ImageRun {
            node,
            style,
            width,
            height,
            margin,
        } = image;
        counters.step(1)?;
        let [top, right, bottom, left] = margin;
        let advance = ((width + left + right) * 64.0).round() as i64;
        let soft = !matches!(
            keyword(
                self.styled.style(style).ok_or(Error::Invalid)?,
                Property::WhiteSpace
            ),
            "pre" | "nowrap"
        );
        if soft
            && (self.pen + advance + 32).div_euclid(64) > i64::from(self.width)
            && !self.pending.is_empty()
        {
            if self.pending.iter().any(|p| !p.space) {
                self.emit(counters, sink, None)?;
            }
            self.restart();
        }
        let above = ((height + top + bottom) * 64.0).round() as i64;
        self.queue(
            Pending {
                node,
                range: 0..0,
                style,
                display: DisplayText::Source,
                pen: 0,
                advance,
                above,
                below: 0,
                space: false,
                tab: false,
                atomic: Some(Atomic::Image(width, height, margin)),
            },
            // Its soft break was taken above; with the pending line
            // emptied or within the width, queue breaks no further.
            soft,
            counters,
            sink,
        )?;
        if soft {
            self.after_space = Some(self.pending.len());
        }
        self.last_space = false;
        self.previous_cr = false;
        Ok(())
    }
    /// An inside marker, the first item of its item's first line: laid
    /// whole, a break opportunity after it where the item wraps, as after
    /// the space its text ends with, and collapsible spaces after it
    /// dropped as at a line's start.
    pub fn marker(
        &mut self,
        marker: &Marker,
        counters: &mut Counters,
        sink: &mut impl LineSink,
    ) -> Result<(), Error> {
        counters.step(1)?;
        let style = self.styled.style(marker.style).ok_or(Error::Invalid)?;
        let soft = white_space(style).2;
        self.queue(
            Pending {
                node: marker.item,
                range: 0..0,
                style: marker.style,
                display: DisplayText::Source,
                pen: 0,
                advance: marker.advance,
                above: marker.above,
                below: marker.below,
                space: false,
                tab: false,
                atomic: Some(Atomic::Marker(marker.kind, marker.ordinal)),
            },
            soft,
            counters,
            sink,
        )?;
        if soft {
            self.after_space = Some(self.pending.len());
        }
        self.last_space = true;
        self.previous_cr = false;
        Ok(())
    }
    pub fn hard_break(
        &mut self,
        style: StyleId,
        counters: &mut Counters,
        sink: &mut impl LineSink,
    ) -> Result<(), Error> {
        let metrics = vertical(
            self.metrics,
            self.styled.style(style).ok_or(Error::Invalid)?,
        )?;
        self.emit(counters, sink, Some(metrics))?;
        self.restart();
        self.last_space = true;
        self.after_newline = true;
        self.previous_cr = false;
        Ok(())
    }
    fn queue(
        &mut self,
        mut item: Pending,
        soft: bool,
        counters: &mut Counters,
        sink: &mut impl LineSink,
    ) -> Result<(), Error> {
        let width = self.width;
        let over = |pen: i64| (pen + item.advance + 32).div_euclid(64) > i64::from(width);
        if !item.space
            && over(self.pen)
            && !self.pending.is_empty()
            && (soft || self.after_space.is_some())
        {
            let keep = self.after_space.unwrap_or(self.pending.len());
            self.break_at(keep, counters, sink)?;
            if soft && over(self.pen) && !self.pending.is_empty() {
                self.emit(counters, sink, None)?;
                self.restart();
            }
        }
        if (self.cut.is_some() || self.pending.len() >= counters.limits.line_scalars)
            && !self.full(&item, soft, counters, sink)?
        {
            return Ok(());
        }
        if item.tab {
            item.advance = tab_advance(self.pen, item.advance);
        }
        item.pen = self.pen;
        self.pen = self.pen.checked_add(item.advance).ok_or(Error::Invalid)?;
        if !coordinate(self.pen as f64 / 64.0) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        let space = item.space;
        self.pending.try_push(item)?;
        if space && soft {
            self.after_space = Some(self.pending.len());
        }
        self.after_newline = false;
        Ok(())
    }
    pub fn finish(
        &mut self,
        counters: &mut Counters,
        sink: &mut impl LineSink,
    ) -> Result<(), Error> {
        if !self.pending.is_empty() || self.after_newline {
            self.emit(counters, sink, None)?;
        }
        self.restart();
        self.after_newline = false;
        Ok(())
    }
    /// At Limits::line_scalars, or past a cut: whether to lay `item`.
    #[cold]
    #[inline(never)]
    fn full(
        &mut self,
        item: &Pending,
        soft: bool,
        counters: &mut Counters,
        sink: &mut impl LineSink,
    ) -> Result<bool, Error> {
        let len = self.pending.len();
        if self.cut.is_none() {
            // A full line breaks at its last wrap opportunity, losing
            // nothing: before a non-space, or before a space past it.
            if let Some(keep) = self.after_space.filter(|k| (1..=len).contains(k)) {
                if keep < len || !item.space {
                    self.break_at(keep, counters, sink)?;
                    return Ok(true);
                }
            }
        }
        if !self.drops(item.space, soft) {
            // A wrap opportunity ends a cut line, or breaks a full one as a
            // word wider than the line is split.
            if self.pending.iter().any(|p| !p.space) {
                self.emit(counters, sink, None)?;
            }
            self.restart();
            return Ok(true);
        }
        self.drop_item(item.node, item.range.start, item.space, soft, counters)?;
        Ok(false)
    }
    /// Whether a full or cut line drops an item rather than ending there:
    /// a non-space ends it where it or a dropped space can wrap.
    fn drops(&self, space: bool, soft: bool) -> bool {
        space || !self.opportunity(soft)
    }
    /// A break may come before the next item: it wraps itself, or a space
    /// before it, laid or dropped, does.
    fn opportunity(&self, soft: bool) -> bool {
        soft || if self.cut.is_some() {
            self.cut_opportunity
        } else {
            self.after_space == Some(self.pending.len())
        }
    }
    /// Cut: drop to the line's end. Spaces alone would hang, so only a
    /// dropped non-space counts the line cut.
    fn drop_item(
        &mut self,
        node: NodeId,
        offset: usize,
        space: bool,
        soft: bool,
        counters: &mut Counters,
    ) -> Result<(), Error> {
        self.cut_opportunity = space && self.opportunity(soft);
        let place = self.cut.unwrap_or(Cut { node, offset });
        self.cut = Some(place);
        if !space && !self.cut_counted {
            self.cut_counted = true;
            counters.cuts.lines = counters.cuts.lines.checked_add(1).ok_or(Error::Invalid)?;
            counters.cuts.first = counters.cuts.first.or(Some(place));
        }
        Ok(())
    }
    /// Ends the line after `keep` pending items and moves the rest, the
    /// unbroken word's tail, to the next.
    #[inline(never)]
    fn break_at(
        &mut self,
        keep: usize,
        counters: &mut Counters,
        sink: &mut impl LineSink,
    ) -> Result<(), Error> {
        self.after_space = None;
        let keep = keep.min(self.pending.len());
        if self
            .pending
            .get(..keep)
            .ok_or(Error::Invalid)?
            .iter()
            .any(|p| !p.space)
        {
            self.emit_prefix(counters, sink, None, keep)?;
        } else if keep > 0 {
            counters.indents = counters.indents.saturating_add(1);
        }
        let remaining = self.pending.len() - keep;
        // Both positions are below the original length: keep <= len,
        // and at < len - keep. Move only the unbroken word's tail.
        for at in 0..remaining {
            self.pending.swap(at, keep + at);
        }
        self.pending.truncate(remaining);
        counters.step(self.pending.len())?;
        // From the new line's start: a tab the tail holds (one laid where
        // its run does not wrap) meets its stop again.
        let mut pen: i64 = 0;
        for p in self.pending.iter_mut() {
            p.pen = pen;
            if p.tab {
                p.advance = tab_advance(pen, self.stops.ok_or(Error::Invalid)?);
            }
            pen = pen.checked_add(p.advance).ok_or(Error::Invalid)?;
        }
        self.pen = pen;
        Ok(())
    }
    /// The tab interval, from the block's space (CSS Text 3 §4.2).
    fn stops(&mut self) -> Result<i64, Error> {
        if let Some(stops) = self.stops {
            return Ok(stops);
        }
        let style = self.styled.style(self.strut).ok_or(Error::Invalid)?;
        let stops = tab_interval(self.metrics, style, letter_spacing(style))?;
        self.stops = Some(stops);
        Ok(stops)
    }
    /// A new line: nothing pending, no break opportunity, no cut.
    fn restart(&mut self) {
        self.pending.clear();
        self.pen = 0;
        self.after_space = None;
        self.cut = None;
        self.cut_counted = false;
        self.cut_opportunity = false;
    }
    fn emit(
        &mut self,
        counters: &mut Counters,
        sink: &mut impl LineSink,
        extra: Option<(i64, i64)>,
    ) -> Result<(), Error> {
        self.emit_prefix(counters, sink, extra, self.pending.len())
    }
    fn emit_prefix(
        &mut self,
        counters: &mut Counters,
        sink: &mut impl LineSink,
        extra: Option<(i64, i64)>,
        count: usize,
    ) -> Result<(), Error> {
        let prefix = self.pending.get(..count).ok_or(Error::Invalid)?;
        counters.step(prefix.len())?;
        if counters.lines >= counters.limits.lines {
            return Err(Error::Limit("MAX_LINES"));
        }
        counters.lines += 1;
        let end = prefix.iter().rposition(|p| !p.space).map_or(0, |i| i + 1);
        let pending = self.pending.get(..end).ok_or(Error::Invalid)?;
        let (above, below) = extra.map_or((self.above, self.below), |(above, below)| {
            (self.above.max(above), self.below.max(below))
        });
        let above = pending.iter().map(|p| p.above).fold(above, i64::max);
        let below = pending.iter().map(|p| p.below).fold(below, i64::max);
        let height = (above + below).max(0) as f64 / 64.0;
        let width = pending
            .last()
            .map_or(0.0, |p| (p.pen + p.advance) as f64 / 64.0);
        let next_y = self.y + height;
        if !coordinate(next_y) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        let mut bounds = LineBounds {
            left: 0.0,
            top: self.y,
            right: 0.0,
            bottom: next_y,
            advance: width,
            y: self.y,
            height,
            baseline: above as f64 / 64.0,
            start: prefix.first().map(|p| p.range.start),
        };
        counters.step(pending.len())?;
        for p in pending {
            let x = p.pen as f64 / 64.0;
            let end = (p.pen + p.advance) as f64 / 64.0;
            bounds.left = bounds.left.min(x).min(end);
            bounds.right = bounds.right.max(x).max(end);
            if let Some(Atomic::Image(width, height, margin)) = p.atomic {
                let [_, _, bottom, left] = margin;
                let image_x = x + left;
                let image_y = self.y + (above as f64 / 64.0 - height - bottom);
                bounds.left = bounds.left.min(image_x);
                bounds.right = bounds.right.max(image_x + width);
                bounds.top = bounds.top.min(image_y);
                bounds.bottom = bounds.bottom.max(image_y + height);
            }
        }
        for value in [
            bounds.left,
            bounds.top,
            bounds.right,
            bounds.bottom,
            bounds.right - bounds.left,
            bounds.bottom - bounds.top,
        ] {
            if !coordinate(value) {
                return Err(Error::Limit("MAX_COORDINATE"));
            }
        }
        sink.measure(bounds, counters)?;
        if sink.wants(self.y, height) {
            let mut fragments: Fragments<TextFragment> = Fragments::new();
            let mut replaced = Fragments::new();
            let mut marker = None;
            for p in pending {
                if let Some(Atomic::Marker(kind, ordinal)) = p.atomic {
                    if counters.fragments >= counters.limits.fragments {
                        return Err(Error::Limit("MAX_FRAGMENTS"));
                    }
                    counters.fragments += 1;
                    let measured = Marker {
                        item: p.node,
                        style: p.style,
                        kind,
                        ordinal,
                        advance: p.advance,
                        above: p.above,
                        below: p.below,
                        scalars: 0,
                    };
                    let end = (p.pen + p.advance) as f64 / 64.0;
                    marker = Some(measured.fragment(end, self.y + above as f64 / 64.0, true));
                    continue;
                }
                if let Some(Atomic::Image(width, height, margin)) = p.atomic {
                    if counters.fragments >= counters.limits.fragments {
                        return Err(Error::Limit("MAX_FRAGMENTS"));
                    }
                    counters.fragments += 1;
                    let [_, _, bottom, left] = margin;
                    replaced.add(
                        ReplacedFragment {
                            node: p.node,
                            style: p.style,
                            x: p.pen as f64 / 64.0 + left,
                            y: above as f64 / 64.0 - height - bottom,
                            width,
                            height,
                        },
                        self.memory,
                        counters,
                    )?;
                    continue;
                }
                if let Some(last) = fragments.last_mut().filter(|f| {
                    f.node == p.node
                        && f.style == p.style
                        && f.display == DisplayText::Source
                        && p.display == DisplayText::Source
                        && f.range.end == p.range.start
                }) {
                    last.range.end = p.range.end;
                    last.width = (p.pen + p.advance) as f64 / 64.0 - last.x;
                } else {
                    if counters.fragments >= counters.limits.fragments {
                        return Err(Error::Limit("MAX_FRAGMENTS"));
                    }
                    counters.fragments += 1;
                    fragments.add(
                        TextFragment {
                            node: p.node,
                            range: p.range.start..p.range.end,
                            style: p.style,
                            display: p.display,
                            x: p.pen as f64 / 64.0,
                            width: p.advance as f64 / 64.0,
                        },
                        self.memory,
                        counters,
                    )?;
                }
            }
            sink.line(
                Line {
                    y: self.y,
                    height,
                    baseline: above as f64 / 64.0,
                    width,
                    fragments,
                    replaced,
                },
                marker,
                counters,
            )?;
        }
        self.y = next_y;
        Ok(())
    }
}
pub(crate) fn coordinate(n: f64) -> bool {
    n.is_finite() && n.abs() <= MAX_COORDINATE
}
/// Replays a text fragment's scalars through `metrics`, which must be the
/// layout's and measure as they did for it; `block` is the style of the
/// block container that laid the fragment's line, whose space sets the tab
/// interval. A tab's advance follows its line's pen, which a fragment does
/// not keep: the fragment's first tab takes what its width leaves, ending
/// on a stop, and each later tab meets the next stop from there.
/// Err(Invalid) for a fragment of another document, or one whose advances
/// do not sum to its width; an untabbed fragment finds that only at its
/// end, after its scalars were reported.
pub fn replay(
    styled: &StyledDocument<'_>,
    metrics: &mut impl FontMetrics,
    fragment: &TextFragment,
    block: StyleId,
    mut each: impl FnMut(Replayed),
) -> Result<(), Error> {
    let text = styled
        .document()
        .node(fragment.node)
        .and_then(|n| n.text())
        .and_then(|s| s.get(fragment.range.start..fragment.range.end))
        .ok_or(Error::Invalid)?;
    let style = styled.style(fragment.style).ok_or(Error::Invalid)?;
    let spacing = letter_spacing(style);
    let units = fragment.width * f64::from(UNIT);
    if !coordinate(fragment.width) || units.fract() != 0.0 {
        return Err(Error::Invalid);
    }
    let width = units as i64;
    let shown = |scalar: char| match fragment.display {
        DisplayText::Source => scalar,
        DisplayText::Space => ' ',
        DisplayText::Replacement => '\u{fffd}',
    };
    let add = |a: i64, b: i64| a.checked_add(b).ok_or(Error::Invalid);
    let mut interval = 0;
    let mut first_tab = None;
    if fragment.display == DisplayText::Source && text.contains('\t') {
        let block = styled.style(block).ok_or(Error::Invalid)?;
        interval = tab_interval(metrics, block, letter_spacing(block))?;
        let mut before: i64 = 0;
        let mut after: Option<i64> = None;
        for scalar in text.chars() {
            after = match (scalar, after) {
                ('\t', None) => Some(0),
                ('\t', Some(pen)) => Some(add(pen, tab_advance(pen, interval))?),
                (c, None) => {
                    before = add(before, advance(metrics, style, c, spacing))?;
                    None
                }
                (c, Some(pen)) => Some(add(pen, advance(metrics, style, c, spacing))?),
            };
        }
        let tab = width
            .checked_sub(add(before, after.unwrap_or(0))?)
            .ok_or(Error::Invalid)?;
        if if interval > 0 {
            !(1..=interval).contains(&tab)
        } else {
            tab != 0
        } {
            return Err(Error::Invalid);
        }
        first_tab = Some(tab);
    }
    let mut pen: i64 = 0;
    let mut origin: i64 = 0;
    for (at, scalar) in text.char_indices() {
        let shown = shown(scalar);
        let advance = if shown != '\t' {
            advance(metrics, style, shown, spacing)
        } else if let Some(tab) = first_tab.take() {
            origin = add(pen, tab)?;
            tab
        } else {
            tab_advance(pen.checked_sub(origin).ok_or(Error::Invalid)?, interval)
        };
        // Within the node's text, which `get` bounded above.
        let start = fragment.range.start + at;
        each(Replayed {
            range: start..start + scalar.len_utf8(),
            shown,
            pen,
            advance,
        });
        pen = add(pen, advance)?;
    }
    if pen != width {
        return Err(Error::Invalid);
    }
    Ok(())
}
/// (preserve spaces, preserve newlines, soft wrap); break-spaces is laid
/// as pre-wrap.
pub(crate) fn white_space(style: &ComputedStyle) -> (bool, bool, bool) {
    let mode = keyword(style, Property::WhiteSpace);
    let preserve = matches!(mode, "pre" | "pre-wrap" | "break-spaces");
    (
        preserve,
        preserve || mode == "pre-line",
        !matches!(mode, "pre" | "nowrap"),
    )
}
pub(crate) fn shown(scalar: char, preserve: bool) -> (char, DisplayText) {
    if preserve && scalar == '\t' {
        ('\t', DisplayText::Source)
    } else if !preserve && matches!(scalar, ' ' | '\t' | '\n' | '\r' | '\x0c') {
        (
            ' ',
            if scalar == ' ' {
                DisplayText::Source
            } else {
                DisplayText::Space
            },
        )
    } else if scalar.is_control() {
        ('\u{fffd}', DisplayText::Replacement)
    } else {
        (scalar, DisplayText::Source)
    }
}
/// letter-spacing in 1/64 CSS px.
pub(crate) fn letter_spacing(style: &ComputedStyle) -> f64 {
    match style.get(Property::LetterSpacing) {
        Some(ComputedValue::Length(Length::Px(n))) => (*n * f64::from(UNIT)).round(),
        _ => 0.0,
    }
}
pub(crate) fn advance(
    metrics: &mut impl FontMetrics,
    style: &ComputedStyle,
    shown: char,
    spacing: f64,
) -> i64 {
    (f64::from(metrics.advance(style, shown)) + spacing).max(0.0) as i64
}
/// TAB_SPACES spaces as this style lays a space, letter-spacing included.
pub(crate) fn tab_interval(
    metrics: &mut impl FontMetrics,
    style: &ComputedStyle,
    spacing: f64,
) -> Result<i64, Error> {
    advance(metrics, style, ' ', spacing)
        .checked_mul(TAB_SPACES)
        .ok_or(Error::Invalid)
}
/// To the next stop after `pen`, measured from the line's start; a zero
/// interval lays a tab with no advance.
pub(crate) fn tab_advance(pen: i64, interval: i64) -> i64 {
    if interval > 0 {
        interval - pen.rem_euclid(interval)
    } else {
        0
    }
}
pub(crate) fn vertical(
    metrics: &mut impl FontMetrics,
    style: &ComputedStyle,
) -> Result<(i64, i64), Error> {
    let font = metrics.line(style);
    if font.baseline > font.height {
        return Err(Error::Invalid);
    }
    let desired = match style.get(Property::LineHeight) {
        Some(ComputedValue::Number(n)) => n * style.font_size() * 64.0,
        Some(ComputedValue::Length(Length::Px(n))) => n * 64.0,
        _ => f64::from(font.height),
    };
    if !desired.is_finite() || !(0.0..=MAX_COORDINATE * 64.0).contains(&desired) {
        return Err(Error::Invalid);
    }
    let height = desired.round() as i64;
    let above = i64::from(font.baseline) + (height - i64::from(font.height)) / 2;
    Ok((above, height - above))
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{BoxKind, Limits};
    use td_css::{Origin, StyleSet, HTML_UA_CSS};
    struct Metrics(u32);
    impl FontMetrics for Metrics {
        fn advance(&mut self, _: &ComputedStyle, _: char) -> u32 {
            self.0
        }
        fn line(&mut self, _: &ComputedStyle) -> FontLine {
            FontLine {
                height: 20 * UNIT,
                baseline: 15 * UNIT,
            }
        }
    }
    fn sheets(css: &str) -> StyleSet {
        StyleSet::new(&[(Origin::UserAgent, HTML_UA_CSS), (Origin::Author, css)]).unwrap()
    }
    fn runs(styled: &StyledDocument<'_>) -> Vec<TextRun> {
        styled
            .boxes()
            .iter()
            .filter(|b| b.kind == BoxKind::Text)
            .map(|b| TextRun {
                node: b.node,
                style: b.style,
                range: 0..styled
                    .document()
                    .node(b.node)
                    .unwrap()
                    .text()
                    .unwrap()
                    .len(),
            })
            .collect()
    }
    fn shown(styled: &StyledDocument<'_>, line: &Line) -> String {
        let mut out = String::new();
        for f in &line.fragments {
            match f.display {
                DisplayText::Source => out.push_str(
                    &styled.document().node(f.node).unwrap().text().unwrap()[f.range.clone()],
                ),
                DisplayText::Space => out.push(' '),
                DisplayText::Replacement => out.push('\u{fffd}'),
            }
        }
        out
    }
    #[test]
    fn prose_break_cases_and_node_boundaries() {
        for (text, width, want) in [
            ("hello world", 7, vec!["hello", "world"]),
            ("abcdefgh", 3, vec!["abc", "def", "gh"]),
            ("  abc", 3, vec!["abc"]),
            ("  abc", 6, vec!["  abc"]),
            ("a   b", 3, vec!["a", "b"]),
            ("a\n", 3, vec!["a", ""]),
            ("\n\n", 3, vec!["", "", ""]),
            // A tab is a preserved space to the next stop, eight columns.
            ("a\tb", 3, vec!["a", "b"]),
            ("a\tb", 9, vec!["a\tb"]),
            ("a\u{a0}b", 2, vec!["a\u{a0}", "b"]),
        ] {
            let html = format!("<div>{text}</div>");
            let dom = td_html::document::parse(&html).unwrap();
            let styled = StyledDocument::new(
                &dom,
                &sheets("div{white-space:pre-wrap}"),
                html.len(),
                Limits::default(),
            )
            .unwrap();
            let runs = runs(&styled);
            let result = wrap_text(&styled, &mut Metrics(64), &runs, width, runs[0].style).unwrap();
            let output: Vec<_> = result.lines.iter().map(|l| shown(&styled, l)).collect();
            assert_eq!(output, want, "{text:?}/{width}");
        }
        let html = "<p>hel<b>lo wo</b>rld";
        let dom = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&dom, &sheets(""), html.len(), Limits::default()).unwrap();
        let runs = runs(&styled);
        let result = wrap_text(&styled, &mut Metrics(64), &runs, 7, runs[0].style).unwrap();
        assert_eq!(
            result
                .lines
                .iter()
                .map(|l| shown(&styled, l))
                .collect::<Vec<_>>(),
            ["hello", "world"]
        );
        assert_eq!(result.lines[0].fragments.len(), 2);
    }
    #[test]
    fn collapse_across_nodes_preserves_utf8_sources_and_controls() {
        let html = "<p> é  <b>\t \n界</b>\t&#1;";
        let dom = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&dom, &sheets(""), html.len(), Limits::default()).unwrap();
        let runs = runs(&styled);
        let result = wrap_text(&styled, &mut Metrics(64), &runs, 80, runs[0].style).unwrap();
        assert_eq!(shown(&styled, &result.lines[0]), "é 界 \u{fffd}");
        for f in &result.lines[0].fragments {
            let text = dom.node(f.node).unwrap().text().unwrap();
            assert!(text.get(f.range.clone()).is_some());
            assert!(!f.range.is_empty());
        }
        assert!(result.lines[0]
            .fragments
            .iter()
            .any(|f| f.display == DisplayText::Space));
        assert!(result.lines[0]
            .fragments
            .iter()
            .any(|f| f.display == DisplayText::Replacement));
    }
    #[test]
    fn fractional_advance_rounds_only_accumulated_width() {
        let dom = td_html::document::parse("<p>abcd").unwrap();
        let styled = StyledDocument::new(&dom, &sheets(""), 7, Limits::default()).unwrap();
        let runs = runs(&styled);
        let result = wrap_text(&styled, &mut Metrics(33), &runs, 1, runs[0].style).unwrap();
        assert_eq!(
            result
                .lines
                .iter()
                .map(|l| shown(&styled, l))
                .collect::<Vec<_>>(),
            ["ab", "cd"]
        );
        assert_eq!(result.lines[0].width, 66.0 / 64.0);
        let result = wrap_text(&styled, &mut Metrics(64), &runs, 0, runs[0].style).unwrap();
        assert_eq!(result.lines.len(), 4);
    }
    #[test]
    fn whitespace_modes_and_empty_input() {
        let html = "<p>a  b\nc";
        let dom = td_html::document::parse(html).unwrap();
        for (mode, want) in [
            ("normal", vec!["a", "b", "c"]),
            ("nowrap", vec!["a b c"]),
            ("pre", vec!["a  b", "c"]),
            ("pre-wrap", vec!["a", "b", "c"]),
            ("pre-line", vec!["a", "b", "c"]),
        ] {
            let styled = StyledDocument::new(
                &dom,
                &sheets(&format!("p{{white-space:{mode}}}")),
                html.len(),
                Limits::default(),
            )
            .unwrap();
            let runs = runs(&styled);
            let result = wrap_text(&styled, &mut Metrics(64), &runs, 1, runs[0].style).unwrap();
            assert_eq!(
                result
                    .lines
                    .iter()
                    .map(|l| shown(&styled, l))
                    .collect::<Vec<_>>(),
                want,
                "{mode}"
            );
            assert!(wrap_text(&styled, &mut Metrics(64), &[], 10, runs[0].style)
                .unwrap()
                .lines
                .is_empty());
        }
    }
    #[test]
    fn bounds_at_edges_and_bad_metrics() {
        let html = "<p>ab";
        let dom = td_html::document::parse(html).unwrap();
        let sheet = sheets("p{white-space:nowrap}");
        // A line that does not wrap is cut at the bound, not refused.
        for (limit, laid, cut) in [(2, "ab", None), (1, "a", Some(1))] {
            let styled = StyledDocument::new(
                &dom,
                &sheet,
                html.len(),
                Limits {
                    line_scalars: limit,
                    ..Limits::default()
                },
            )
            .unwrap();
            let runs = runs(&styled);
            let result = wrap_text(&styled, &mut Metrics(64), &runs, 10, runs[0].style).unwrap();
            assert_eq!(shown(&styled, &result.lines[0]), laid);
            assert_eq!(result.cuts.lines, usize::from(cut.is_some()));
            assert_eq!(result.cuts.first.map(|c| (c.node, c.offset)), {
                cut.map(|offset| (runs[0].node, offset))
            });
        }
        let styled = StyledDocument::new(&dom, &sheet, html.len(), Limits::default()).unwrap();
        let runs = runs(&styled);
        let measured = wrap_text(&styled, &mut Metrics(64), &runs, 10, runs[0].style).unwrap();
        for (limit, ok) in [(measured.work, true), (measured.work - 1, false)] {
            let styled = StyledDocument::new(
                &dom,
                &sheet,
                html.len(),
                Limits {
                    work: limit,
                    ..Limits::default()
                },
            )
            .unwrap();
            assert_eq!(
                wrap_text(&styled, &mut Metrics(64), &runs, 10, runs[0].style).is_ok(),
                ok
            );
        }
        for (limit, ok) in [
            (measured.charged_bytes, true),
            (measured.charged_bytes - 1, false),
        ] {
            let styled = StyledDocument::new(
                &dom,
                &sheet,
                html.len(),
                Limits {
                    bytes: limit,
                    ..Limits::default()
                },
            )
            .unwrap();
            assert_eq!(
                wrap_text(&styled, &mut Metrics(64), &runs, 10, runs[0].style).is_ok(),
                ok
            );
        }
        for (lines, fragments, ok) in [(1, 1, true), (0, 1, false), (1, 0, false)] {
            let styled = StyledDocument::new(
                &dom,
                &sheet,
                html.len(),
                Limits {
                    lines,
                    fragments,
                    ..Limits::default()
                },
            )
            .unwrap();
            assert_eq!(
                wrap_text(&styled, &mut Metrics(64), &runs, 10, runs[0].style).is_ok(),
                ok
            );
        }
        struct Bad;
        impl FontMetrics for Bad {
            fn advance(&mut self, _: &ComputedStyle, _: char) -> u32 {
                64
            }
            fn line(&mut self, _: &ComputedStyle) -> FontLine {
                FontLine {
                    height: 0,
                    baseline: 1,
                }
            }
        }
        assert!(matches!(
            wrap_text(&styled, &mut Bad, &runs, 10, runs[0].style),
            Err(Error::Invalid)
        ));
    }
    // Whole-word lookahead oracle: unlike Flow it never moves a partial word.
    // A column is a scalar; a tab runs to the next multiple of eight.
    fn reference(text: &str, width: usize) -> Vec<String> {
        let capacity = width.max(1);
        let mut output = Vec::new();
        for paragraph in text.split('\n') {
            let chars: Vec<_> = paragraph.chars().collect();
            let mut line = String::new();
            let mut column = 0;
            let mut gap = String::new();
            let mut gap_end = 0;
            let mut at = 0;
            while at < chars.len() {
                if matches!(chars[at], ' ' | '\t') {
                    if gap.is_empty() {
                        gap_end = column;
                    }
                    gap_end = if chars[at] == '\t' {
                        (gap_end / 8 + 1) * 8
                    } else {
                        gap_end + 1
                    };
                    gap.push(chars[at]);
                    at += 1;
                    continue;
                }
                let begin = at;
                while at < chars.len() && !matches!(chars[at], ' ' | '\t') {
                    at += 1;
                }
                let word = &chars[begin..at];
                let start = if gap.is_empty() { column } else { gap_end };
                if start + word.len() > width {
                    if !line.is_empty() {
                        output.push(std::mem::take(&mut line));
                    }
                    column = 0;
                } else {
                    line.push_str(&gap);
                    column = start;
                }
                gap.clear();
                for scalar in word {
                    if column == capacity {
                        output.push(std::mem::take(&mut line));
                        column = 0;
                    }
                    line.push(*scalar);
                    column += 1;
                }
            }
            output.push(line);
        }
        output
    }
    #[test]
    fn generated_pre_wrap_matches_whole_word_oracle() {
        let sheet = sheets("pre{white-space:pre-wrap}");
        let alphabet = ['a', 'b', ' ', ' ', '\t', '\n', 'é', '界', '\u{a0}'];
        let mut seed = 7u64;
        for _ in 0..160 {
            let mut text = String::from("x");
            for _ in 0..60 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                text.push(alphabet[(seed as usize) % alphabet.len()]);
            }
            let html = format!("<pre>{text}</pre>");
            let dom = td_html::document::parse(&html).unwrap();
            let styled = StyledDocument::new(&dom, &sheet, html.len(), Limits::default()).unwrap();
            let runs = runs(&styled);
            for width in [1, 2, 3, 5, 8, 13, 21, 80] {
                let actual =
                    wrap_text(&styled, &mut Metrics(64), &runs, width, runs[0].style).unwrap();
                assert_eq!(
                    actual
                        .lines
                        .iter()
                        .map(|l| shown(&styled, l))
                        .collect::<Vec<_>>(),
                    reference(&text, width as usize),
                    "{text:?} width {width}"
                );
            }
        }
    }

    #[test]
    fn line_scratch_global_edge_and_invalid_utf8_ranges() {
        let sheet = sheets("p{white-space:nowrap}");
        // Past the bound a line that does not wrap is cut there, the rest
        // of it not laid.
        for (count, cut) in [
            (crate::MAX_LINE_SCALARS, false),
            (crate::MAX_LINE_SCALARS + 1, true),
            (4 * crate::MAX_LINE_SCALARS, true),
        ] {
            let html = format!("<p>{}", "é".repeat(count));
            let dom = td_html::document::parse(&html).unwrap();
            let styled = StyledDocument::new(&dom, &sheet, html.len(), Limits::default()).unwrap();
            let runs = runs(&styled);
            let result = wrap_text(&styled, &mut Metrics(0), &runs, 0, runs[0].style).unwrap();
            assert_eq!(result.lines.len(), 1);
            let laid = 2 * crate::MAX_LINE_SCALARS;
            assert_eq!(result.lines[0].fragments.len(), 1);
            assert_eq!(result.lines[0].fragments[0].range, 0..laid);
            assert_eq!(result.cuts.lines, usize::from(cut));
            assert_eq!(
                result.cuts.first,
                cut.then_some(Cut {
                    node: runs[0].node,
                    offset: laid
                })
            );
        }
        let dom = td_html::document::parse("<p>é").unwrap();
        let styled = StyledDocument::new(&dom, &sheet, 5, Limits::default()).unwrap();
        let mut runs = runs(&styled);
        runs[0].range = 1..2;
        assert!(matches!(
            wrap_text(&styled, &mut Metrics(64), &runs, 10, runs[0].style),
            Err(Error::Invalid)
        ));
        assert!(coordinate(MAX_COORDINATE));
        assert!(coordinate(-MAX_COORDINATE));
        assert!(!coordinate(MAX_COORDINATE + 1.0));
        assert!(!coordinate(f64::NAN));
    }
    /// Narrow and wide glyphs: a space 8 px, so stops every 64 px.
    struct Widths;
    impl FontMetrics for Widths {
        fn advance(&mut self, _: &ComputedStyle, c: char) -> u32 {
            match c {
                'i' => 4 * UNIT,
                'm' => 12 * UNIT,
                c if u32::from(c) > 255 => 16 * UNIT,
                _ => 8 * UNIT,
            }
        }
        fn line(&mut self, _: &ComputedStyle) -> FontLine {
            FontLine {
                height: 20 * UNIT,
                baseline: 15 * UNIT,
            }
        }
    }
    fn wrapped(
        html: &str,
        css: &str,
        limits: Limits<'static>,
        metrics: &mut impl FontMetrics,
        width: u32,
    ) -> (td_html::document::Document, WrappedText) {
        let dom = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&dom, &sheets(css), html.len(), limits).unwrap();
        let runs = runs(&styled);
        let result = wrap_text(&styled, metrics, &runs, width, runs[0].style).unwrap();
        drop(styled);
        (dom, result)
    }
    /// Each scalar a line lays: its bytes, as shown, x and advance in px.
    fn replayed(
        styled: &StyledDocument<'_>,
        metrics: &mut impl FontMetrics,
        line: &Line,
        block: StyleId,
    ) -> Vec<(Range<usize>, char, f64, f64)> {
        let mut out = Vec::new();
        for f in &line.fragments {
            replay(styled, metrics, f, block, |s| {
                out.push((
                    s.range,
                    s.shown,
                    f.x + s.pen as f64 / 64.0,
                    s.advance as f64 / 64.0,
                ))
            })
            .unwrap();
        }
        out
    }
    fn lines_of(html: &str, css: &str, metrics: &mut impl FontMetrics, width: u32) -> Vec<String> {
        let dom = td_html::document::parse(html).unwrap();
        let styled =
            StyledDocument::new(&dom, &sheets(css), html.len(), Limits::default()).unwrap();
        let runs = runs(&styled);
        let result = wrap_text(&styled, metrics, &runs, width, runs[0].style).unwrap();
        result.lines.iter().map(|l| shown(&styled, l)).collect()
    }
    #[test]
    fn tabs_advance_to_stops_from_the_line_start() {
        for (text, x) in [
            ("\tx", 64.0),
            ("a\tx", 64.0),
            ("abcdefg\tx", 64.0),
            ("abcdefgh\tx", 128.0),
            ("界界界\tx", 64.0),
            ("界界界界\tx", 128.0),
            ("\t\tx", 128.0),
            ("a\t\tx", 128.0),
            ("ab\tcd\tx", 128.0),
            ("iii\tx", 64.0),
            ("mmmmmm\tx", 128.0),
            ("iiiiiiiiiiiiiiii\tx", 128.0),
            ("mi界\tx", 64.0),
        ] {
            let html = format!("<pre>{text}</pre>");
            let dom = td_html::document::parse(&html).unwrap();
            for css in [
                "",
                "pre{white-space:pre-wrap}",
                "pre{white-space:break-spaces}",
            ] {
                let styled =
                    StyledDocument::new(&dom, &sheets(css), html.len(), Limits::default()).unwrap();
                let runs = runs(&styled);
                let result = wrap_text(&styled, &mut Widths, &runs, 1000, runs[0].style).unwrap();
                let line = &result.lines[0];
                // One source fragment, however many tabs.
                assert_eq!(line.fragments.len(), 1, "{text:?}");
                assert_eq!(line.fragments[0].display, DisplayText::Source);
                assert_eq!(line.fragments[0].range, 0..text.len());
                assert_eq!(line.width, x + 8.0, "{text:?} {css}");
                let scalars = replayed(&styled, &mut Widths, line, runs[0].style);
                assert_eq!(scalars.last().unwrap().2, x, "{text:?}");
                for (range, shown, at, advance) in &scalars {
                    assert_eq!(
                        text.get(range.clone()).unwrap().chars().next(),
                        Some(*shown)
                    );
                    if *shown == '\t' {
                        assert!(*advance > 0.0 && *advance <= 64.0);
                        assert_eq!((at + advance) % 64.0, 0.0, "{text:?}");
                    }
                }
            }
        }
        // Trailing tabs, as trailing preserved spaces, are not laid.
        let (_, result) = wrapped(
            "<pre>ab\t\t</pre>",
            "",
            Limits::default(),
            &mut Widths,
            1000,
        );
        assert_eq!(result.lines[0].width, 16.0);
        assert_eq!(result.lines[0].fragments[0].range, 0..2);
    }
    #[test]
    fn tabs_wrap_as_preserved_spaces_only_where_lines_wrap() {
        // A column a pixel, stops every eight.
        for (css, text, width, want) in [
            ("pre-wrap", "ab\tcd\tef", 10, vec!["ab\tcd", "ef"]),
            ("break-spaces", "ab\tcd\tef", 10, vec!["ab\tcd", "ef"]),
            ("pre", "ab\tcd\tef", 10, vec!["ab\tcd\tef"]),
            ("pre-wrap", "\t\t\tab", 10, vec!["ab"]),
            (
                "pre-wrap",
                "aaaaaaaaaaaa bb\tc",
                10,
                vec!["aaaaaaaaaa", "aa bb\tc"],
            ),
            ("pre-wrap", "a\tb\nc\td", 100, vec!["a\tb", "c\td"]),
            // Where white space collapses a tab is a collapsed space.
            ("normal", "a\t\tb", 100, vec!["a b"]),
            ("pre-line", "a\t\tb\n\tc", 100, vec!["a b", "c"]),
            ("nowrap", "a\tb", 1, vec!["a b"]),
        ] {
            let html = format!("<pre>{text}</pre>");
            let css = format!("pre{{white-space:{css}}}");
            assert_eq!(
                lines_of(&html, &css, &mut Metrics(64), width),
                want,
                "{css} {text:?}"
            );
        }
        // After a wrap the stops are measured from the new line's start.
        let html = "<pre>aaaaaaaaaaaa bb\tc</pre>";
        let dom = td_html::document::parse(html).unwrap();
        let css = sheets("pre{white-space:pre-wrap}");
        let styled = StyledDocument::new(&dom, &css, html.len(), Limits::default()).unwrap();
        let runs = runs(&styled);
        let result = wrap_text(&styled, &mut Metrics(64), &runs, 10, runs[0].style).unwrap();
        let scalars = replayed(&styled, &mut Metrics(64), &result.lines[1], runs[0].style);
        assert_eq!(scalars[5], (15..16, '\t', 5.0, 3.0));
        assert_eq!(scalars[6], (16..17, 'c', 8.0, 1.0));
        // A collapsed tab is a space fragment a space wide, as before.
        let (dom, result) = wrapped("<p>a\tb", "", Limits::default(), &mut Metrics(64), 100);
        let text = dom
            .node(result.lines[0].fragments[1].node)
            .unwrap()
            .text()
            .unwrap();
        let fragment = &result.lines[0].fragments[1];
        assert_eq!(fragment.display, DisplayText::Space);
        assert_eq!(&text[fragment.range.clone()], "\t");
        assert_eq!((fragment.x, fragment.width), (1.0, 1.0));
        assert_eq!(result.lines[0].fragments.len(), 3);
    }
    #[test]
    fn a_tab_a_run_that_does_not_wrap_moves_meets_its_stop_again() {
        // The pre run's tab is laid from x 3 to 8; when the run moves to a
        // new line it is laid from 1 to 8, so the line is 9 wide.
        let lines = lines_of(
            "<p>a <span>b\tc</span>d",
            "span{white-space:pre}",
            &mut Metrics(64),
            9,
        );
        assert_eq!(lines, ["a", "b\tc", "d"]);
        let (_, result) = wrapped(
            "<p>a <span>b\tc</span>d",
            "span{white-space:pre}",
            Limits::default(),
            &mut Metrics(64),
            9,
        );
        assert_eq!(result.lines[1].width, 9.0);
        assert_eq!(result.lines[1].fragments[0].width, 9.0);
    }
    #[test]
    fn a_line_holds_one_fragment_however_many_tabs_and_each_maps_to_its_byte() {
        let css = sheets("pre{white-space:pre-wrap}");
        for tabs in [0, 1, 2, 4, 9, 40] {
            let mut text = String::from("a");
            for _ in 0..tabs {
                text.push_str("\tb");
            }
            let html = format!("<pre>{text}\n{text}</pre>");
            let dom = td_html::document::parse(&html).unwrap();
            let styled = StyledDocument::new(&dom, &css, html.len(), Limits::default()).unwrap();
            let runs = runs(&styled);
            let source = dom.node(runs[0].node).unwrap().text().unwrap();
            let result = wrap_text(&styled, &mut Widths, &runs, 100_000, runs[0].style).unwrap();
            assert_eq!(result.lines.len(), 2);
            for line in &result.lines {
                assert_eq!(line.fragments.len(), 1, "{tabs} tabs");
                let scalars = replayed(&styled, &mut Widths, line, runs[0].style);
                assert_eq!(scalars.len(), 1 + 2 * tabs);
                // Hit-testing: every x of the line falls on one scalar,
                // a tab's on the tab's own byte.
                let mut x = 0.0;
                while x < line.width {
                    let (range, shown, _, _) = scalars
                        .iter()
                        .find(|(_, _, at, advance)| *at <= x && x < at + advance)
                        .unwrap();
                    assert_eq!(source.as_bytes()[range.start] == b'\t', *shown == '\t');
                    assert_eq!(range.len(), 1);
                    x += 4.0;
                }
            }
        }
        // Another run, another fragment: one a node, not one a tab.
        let (_, result) = wrapped(
            "<pre>a\t<b>b\tb</b>\tc\td",
            "",
            Limits::default(),
            &mut Widths,
            1000,
        );
        assert_eq!(result.lines[0].fragments.len(), 3);
    }
    #[test]
    fn replay_refuses_other_metrics_and_other_documents() {
        let html = "<pre>ab\tc</pre>";
        let dom = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&dom, &sheets(""), html.len(), Limits::default()).unwrap();
        let runs = runs(&styled);
        let result = wrap_text(&styled, &mut Metrics(64), &runs, 100, runs[0].style).unwrap();
        let fragment = &result.lines[0].fragments[0];
        assert_eq!(
            replayed(&styled, &mut Metrics(64), &result.lines[0], runs[0].style),
            [
                (0..1, 'a', 0.0, 1.0),
                (1..2, 'b', 1.0, 1.0),
                (2..3, '\t', 2.0, 6.0),
                (3..4, 'c', 8.0, 1.0)
            ]
        );
        // Advances that cannot sum to the width are refused; a first tab
        // takes up what it can, so a tab can hide wider ones.
        assert!(matches!(
            replay(&styled, &mut Metrics(32), fragment, runs[0].style, |_| {}),
            Err(Error::Invalid)
        ));
        let (dom, plain) = wrapped("<pre>ab", "", Limits::default(), &mut Metrics(64), 100);
        let plain_styled = StyledDocument::new(&dom, &sheets(""), 7, Limits::default()).unwrap();
        assert!(matches!(
            replay(
                &plain_styled,
                &mut Metrics(128),
                &plain.lines[0].fragments[0],
                plain.lines[0].fragments[0].style,
                |_| {}
            ),
            Err(Error::Invalid)
        ));
        let mut other = fragment.clone();
        other.range = 0..9;
        assert!(matches!(
            replay(&styled, &mut Metrics(64), &other, runs[0].style, |_| {}),
            Err(Error::Invalid)
        ));
        // A width that leaves the first tab nothing, or more than an
        // interval, is no layout's.
        for width in [3.0, 12.0] {
            let mut other = fragment.clone();
            other.width = width;
            assert!(matches!(
                replay(&styled, &mut Metrics(64), &other, runs[0].style, |_| {}),
                Err(Error::Invalid)
            ));
        }
        let mut other = fragment.clone();
        other.width = 11.0;
        assert!(replay(&styled, &mut Metrics(64), &other, runs[0].style, |_| {}).is_ok());
    }
    #[test]
    fn lines_past_the_scalar_bound_are_cut_or_broken_never_refused() {
        let four = Limits {
            line_scalars: 4,
            ..Limits::default()
        };
        // (html, css, lines, cut lines, the first cut's byte)
        for (html, css, want, cut, first) in [
            // A line that does not wrap is cut to its end.
            ("<pre>aaaaaa\nbb", "", vec!["aaaa", "bb"], 1, Some(4)),
            ("<pre>a\taaaa\tb", "", vec!["a\taa"], 1, Some(4)),
            (
                "<p>aaaaaa bb",
                "p{white-space:nowrap}",
                vec!["aaaa"],
                1,
                Some(4),
            ),
            // Spaces alone would have hung: dropped, not counted a cut.
            ("<pre>aaaa  \t\nb", "", vec!["aaaa", "b"], 0, None),
            ("<pre>aaaa  b\nc", "", vec!["aaaa", "c"], 1, Some(4)),
            // A line that wraps breaks at the bound instead.
            (
                "<pre>aaaaaaaaaa",
                "pre{white-space:pre-wrap}",
                vec!["aaaa", "aaaa", "aa"],
                0,
                None,
            ),
            ("<p>aaaaaaaaaa", "", vec!["aaaa", "aaaa", "aa"], 0, None),
            (
                "<pre>          x",
                "pre{white-space:pre-wrap}",
                vec!["x"],
                0,
                None,
            ),
            // A wrap opportunity ends a cut line.
            (
                "<p><span>aaaaaa</span> bb",
                "span{white-space:nowrap}",
                vec!["aaaa", "bb"],
                1,
                Some(4),
            ),
        ] {
            let (dom, result) = wrapped(html, css, four, &mut Metrics(0), 100);
            let styled = StyledDocument::new(&dom, &sheets(css), html.len(), four).unwrap();
            let lines: Vec<_> = result.lines.iter().map(|l| shown(&styled, l)).collect();
            assert_eq!(lines, want, "{html}");
            assert_eq!(result.cuts.lines, cut, "{html}");
            assert_eq!(result.cuts.first.map(|c| c.offset), first, "{html}");
            if let Some(cut) = result.cuts.first {
                let text = dom.node(cut.node).unwrap().text().unwrap();
                // The cut's place is the first scalar no fragment holds.
                assert!(text.is_char_boundary(cut.offset));
                for line in &result.lines {
                    for f in &line.fragments {
                        assert!(f.node != cut.node || !f.range.contains(&cut.offset));
                    }
                }
            }
        }
        // At the default bound: pre cuts both lines; pre-wrap wraps the
        // first, and its run of spaces past the bound hangs, unrefused.
        let html = format!("<pre>{}\n{}x", "a".repeat(70_000), " ".repeat(70_000));
        for (css, cut, last) in [("", 2, ""), ("pre{white-space:pre-wrap}", 0, "x")] {
            let (dom, result) = wrapped(&html, css, Limits::default(), &mut Metrics(64), 1000);
            assert_eq!(result.cuts.lines, cut);
            let styled =
                StyledDocument::new(&dom, &sheets(css), html.len(), Limits::default()).unwrap();
            let shown = shown(&styled, result.lines.last().unwrap());
            assert_eq!(shown.trim_start_matches(' '), last);
        }
    }
    /// One pixel a scalar, but `m`, three.
    struct Glyphs;
    impl FontMetrics for Glyphs {
        fn advance(&mut self, _: &ComputedStyle, c: char) -> u32 {
            if c == 'm' {
                3 * UNIT
            } else {
                UNIT
            }
        }
        fn line(&mut self, _: &ComputedStyle) -> FontLine {
            FontLine {
                height: 20 * UNIT,
                baseline: 15 * UNIT,
            }
        }
    }
    #[test]
    fn a_full_or_cut_line_ends_at_a_wrap_opportunity_rather_than_dropping() {
        let four = Limits {
            line_scalars: 4,
            ..Limits::default()
        };
        // (html, css, width, lines, cut lines, the first cut's byte)
        for (html, css, width, want, cut, first) in [
            // A space dropped past a cut is a wrap opportunity: the runs
            // that do not wrap after it start a new line, not lost.
            (
                "<p><i>aaaaaa</i> <i>bb</i> <i>cc</i> dd</p>",
                "i{white-space:nowrap}",
                400,
                vec!["aaaa", "bb", "cc", "dd"],
                1,
                Some(4),
            ),
            // A full line's trailing preserved spaces hang; the run that
            // does not wrap after them starts the next line, uncut.
            (
                "<div>a     <i>XYZ</i> end</div>",
                "div{white-space:pre-wrap}i{white-space:nowrap}",
                40,
                vec!["a", "XYZ", "end"],
                0,
                None,
            ),
            // A full line breaks at its last opportunity, so the tail
            // that moves keeps its room and is cut only when full again.
            (
                "<p>x <i>aaaam</i>",
                "i{white-space:nowrap}",
                5,
                vec!["x", "aaaa"],
                1,
                Some(4),
            ),
            // The same before a space: the space follows the tail.
            (
                "<p>x <i>aa</i><b> a</b>",
                "i{white-space:nowrap}b{white-space:pre}",
                100,
                vec!["x", "aa a"],
                0,
                None,
            ),
        ] {
            let (dom, result) = wrapped(html, css, four, &mut Glyphs, width);
            let styled = StyledDocument::new(&dom, &sheets(css), html.len(), four).unwrap();
            let lines: Vec<_> = result.lines.iter().map(|l| shown(&styled, l)).collect();
            assert_eq!(lines, want, "{html}");
            assert_eq!(result.cuts.lines, cut, "{html}");
            assert_eq!(result.cuts.first.map(|c| c.offset), first, "{html}");
            // A counted cut names a place in a line it did not lay.
            if let Some(cut) = result.cuts.first {
                for line in &result.lines {
                    for f in &line.fragments {
                        assert!(f.node != cut.node || f.range.end <= cut.offset);
                    }
                }
            }
        }
        // At the default bound, a preserved run of spaces past it, then a
        // run that does not wrap.
        let html = format!("<div>a{}<i>XYZ</i>", " ".repeat(65_540));
        let css = "div{white-space:pre-wrap}i{white-space:nowrap}";
        let (dom, result) = wrapped(&html, css, Limits::default(), &mut Glyphs, 1280);
        let styled =
            StyledDocument::new(&dom, &sheets(css), html.len(), Limits::default()).unwrap();
        let lines: Vec<_> = result.lines.iter().map(|l| shown(&styled, l)).collect();
        assert_eq!(lines, ["a", "XYZ"]);
        assert_eq!(result.cuts, Cuts::default());
        // A bound of zero is clamped to one: every line makes progress.
        let zero = Limits {
            line_scalars: 0,
            ..Limits::default()
        };
        let (dom, result) = wrapped("<p>ab", "p{white-space:nowrap}", zero, &mut Glyphs, 10);
        let styled = StyledDocument::new(&dom, &sheets(""), 5, zero).unwrap();
        assert_eq!(styled.limits().line_scalars, 1);
        assert_eq!(shown(&styled, &result.lines[0]), "a");
        assert_eq!(result.cuts.lines, 1);
    }
    #[test]
    fn the_block_sets_the_tab_interval_with_its_letter_spacing() {
        // Each scalar 1 px and 1 px apart: a space 2 px, stops every 16.
        let css = "pre{letter-spacing:1px}";
        let (_, result) = wrapped("<pre>a\tb", css, Limits::default(), &mut Metrics(64), 100);
        assert_eq!(result.lines[0].width, 18.0);
        // The tab a moved tail holds meets the same stops again.
        let css = "p{letter-spacing:1px}span{white-space:pre}";
        let html = "<p>a <span>b\tc</span>d";
        let (_, result) = wrapped(html, css, Limits::default(), &mut Metrics(64), 18);
        assert_eq!(result.lines.len(), 3);
        assert_eq!(result.lines[1].width, 18.0);
    }
    #[test]
    fn replay_finds_a_stop_for_a_fragment_starting_off_one() {
        let html = "<pre>a<b>b\tc\td</b></pre>";
        let dom = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&dom, &sheets(""), html.len(), Limits::default()).unwrap();
        let runs = runs(&styled);
        let result = wrap_text(&styled, &mut Widths, &runs, 1000, runs[0].style).unwrap();
        let line = &result.lines[0];
        assert_eq!(line.fragments.len(), 2);
        let scalars = replayed(&styled, &mut Widths, line, runs[0].style);
        let at: Vec<_> = scalars.iter().map(|s| (s.1, s.2, s.3)).collect();
        assert_eq!(
            at,
            [
                ('a', 0.0, 8.0),
                ('b', 8.0, 8.0),
                ('\t', 16.0, 48.0),
                ('c', 64.0, 8.0),
                ('\t', 72.0, 56.0),
                ('d', 128.0, 8.0)
            ]
        );
    }
    #[test]
    fn mixed_modes_preserve_break_eligibility_and_space_identity() {
        for (html, css, width, want) in [
            (
                "<p><span>a b</span>c",
                "span{white-space:nowrap}",
                2,
                vec!["a b", "c"],
            ),
            (
                "<p>a <span>b c</span>d",
                "span{white-space:nowrap}",
                3,
                vec!["a", "b c", "d"],
            ),
            (
                "<p>a <span>bcd</span>",
                "span{white-space:nowrap}",
                4,
                vec!["a", "bcd"],
            ),
            (
                "<p><span>a b</span>c",
                "span{white-space:pre}",
                2,
                vec!["a b", "c"],
            ),
            (
                "<p><span>  </span> b",
                "span{white-space:pre-wrap}",
                10,
                vec!["   b"],
            ),
        ] {
            let dom = td_html::document::parse(html).unwrap();
            let styled =
                StyledDocument::new(&dom, &sheets(css), html.len(), Limits::default()).unwrap();
            let runs = runs(&styled);
            let result = wrap_text(&styled, &mut Metrics(64), &runs, width, runs[0].style).unwrap();
            assert_eq!(
                result
                    .lines
                    .iter()
                    .map(|l| shown(&styled, l))
                    .collect::<Vec<_>>(),
                want,
                "{html}"
            );
        }
    }
    #[test]
    fn preserved_newline_contributes_its_own_font_metrics() {
        struct SizedMetrics;
        impl FontMetrics for SizedMetrics {
            fn advance(&mut self, s: &ComputedStyle, _: char) -> u32 {
                (s.font_size() * 32.0) as u32
            }
            fn line(&mut self, s: &ComputedStyle) -> FontLine {
                FontLine {
                    height: (s.font_size() * 64.0) as u32,
                    baseline: (s.font_size() * 48.0) as u32,
                }
            }
        }
        let html = "<p>x<span>&#10;</span>y";
        let dom = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(
            &dom,
            &sheets("span{white-space:pre;font-size:100px}"),
            html.len(),
            Limits::default(),
        )
        .unwrap();
        let runs = runs(&styled);
        let result = wrap_text(&styled, &mut SizedMetrics, &runs, 1000, runs[0].style).unwrap();
        assert_eq!(result.lines.len(), 2);
        assert_eq!(result.lines[0].height, 100.0);
        assert_eq!(result.lines[1].height, 16.0);
        assert_eq!(result.lines[1].y, 100.0);
    }
}
