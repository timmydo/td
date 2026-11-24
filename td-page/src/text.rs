//! Places in a laid page and the text between them (td-web/DESIGN.md §10,
//! "Places, find, selection and links"): a place is a DOM position, a
//! paragraph the lines one block lays up to a line ended by a newline, a
//! `br` or its block (`Line::end`), past the lines of floats and
//! inline-blocks laid between them, and the text find searches and a copy
//! takes is the fragments' displayed text in tree order with the white
//! space a `Wrap` leaves between lines put back.

use std::ops::ControlFlow;

use crate::fonts::Measure;
use crate::page::{Laid, Span, CHUNK};
use crate::tree::NONE;
use td_alloc::{Budget, BudgetError, BudgetString, BudgetVec};
use td_css::values::{ComputedValue, Property};
use td_html::document::NodeId;
use td_layout::inline::{
    replay_until, DisplayText, Line, LineEnd, ReplacedFragment, TextFragment, UNIT,
};
use td_layout::{BoxKind, StyleId, TableRole};

/// A DOM position: a node's place in tree order and a byte offset in its
/// text, or for a replaced element 0 before it and 1 after. Ordered by
/// tree order, then offset.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Place {
    pub rank: u32,
    pub offset: u32,
    pub node: Option<NodeId>,
}

impl Place {
    pub fn new(laid: &Laid, node: NodeId, offset: usize) -> Self {
        Self {
            rank: laid.tree().rank(node),
            offset: u32::try_from(offset).unwrap_or(u32::MAX),
            node: Some(node),
        }
    }
    /// Past every place in a page.
    pub const END: Self = Self {
        rank: NONE,
        offset: u32::MAX,
        node: None,
    };
}

/// A line's piece: a text fragment or a replaced one, by its index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Piece {
    Text(usize),
    Replaced(usize),
}

/// A piece with its places.
#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub piece: Piece,
    pub start: Place,
    pub end: Place,
}

fn text_item(laid: &Laid, at: usize, f: &TextFragment) -> Item {
    Item {
        piece: Piece::Text(at),
        start: Place::new(laid, f.node, f.range.start),
        end: Place::new(laid, f.node, f.range.end),
    }
}

/// A replaced element's places, before it and after it; an inline-block's
/// placeholder's end is past every place in its subtree, so the line
/// holding it holds its lines, and before any other, so no piece after
/// it starts there: the last node of its subtree at an offset no piece
/// reaches (td-web/DESIGN.md §10, "Places").
fn replaced_item(laid: &Laid, at: usize, r: &ReplacedFragment) -> Item {
    let end = if r.atomic {
        let last = laid.tree().last_within(r.node);
        Place {
            rank: laid.tree().rank(last),
            offset: u32::MAX,
            node: Some(last),
        }
    } else {
        Place::new(laid, r.node, 1)
    };
    Item {
        piece: Piece::Replaced(at),
        start: Place::new(laid, r.node, 0),
        end,
    }
}

/// A line's pieces in tree order.
pub fn items<'a>(laid: &'a Laid, line: &'a Line) -> impl Iterator<Item = Item> + 'a {
    let mut texts = line.fragments.iter().enumerate().peekable();
    let mut replaced = line.replaced.iter().enumerate().peekable();
    std::iter::from_fn(move || {
        let text = texts.peek().map(|&(at, f)| text_item(laid, at, f));
        let image = replaced.peek().map(|&(at, r)| replaced_item(laid, at, r));
        match (text, image) {
            (Some(text), Some(image)) if image.start < text.start => {
                replaced.next();
                Some(image)
            }
            (Some(text), _) => {
                texts.next();
                Some(text)
            }
            (None, Some(image)) => {
                replaced.next();
                Some(image)
            }
            (None, None) => None,
        }
    })
}

/// A line's first and last places; none for a line with no piece.
pub fn bounds(laid: &Laid, line: &Line) -> Option<(Place, Place)> {
    let mut all = items(laid, line);
    let first = all.next()?;
    let last = all.last().unwrap_or(first);
    Some((first.start, last.end))
}

/// Whether a style paints nothing it lays (`visibility: hidden` or
/// `collapse`): such text is not found, selected or copied.
pub fn hidden(laid: &Laid, style: StyleId) -> bool {
    matches!(
        laid.styled()
            .style(style)
            .and_then(|s| s.get(Property::Visibility)),
        Some(ComputedValue::Keyword("hidden" | "collapse"))
    )
}

/// A line's pieces that are painted, in tree order.
fn shown_items<'a>(laid: &'a Laid, line: &'a Line) -> impl Iterator<Item = Item> + 'a {
    items(laid, line).filter(move |item| {
        let style = match item.piece {
            Piece::Text(i) => line.fragments.get(i).map(|f| f.style),
            Piece::Replaced(i) => line.replaced.get(i).map(|r| r.style),
        };
        style.is_some_and(|style| !hidden(laid, style))
    })
}

/// The text a text fragment's node holds in its range.
pub fn fragment_text<'a>(laid: &'a Laid, f: &TextFragment) -> &'a str {
    laid.tree()
        .document()
        .node(f.node)
        .and_then(|node| node.text())
        .and_then(|text| text.get(f.range.start..f.range.end))
        .unwrap_or("")
}

/// Whether a line ends its paragraph.
pub fn ends_paragraph(line: &Line) -> bool {
    matches!(line.end(), LineEnd::Newline | LineEnd::Break | LineEnd::End)
}

/// The line before `at` in its paragraph, if any: the last line its
/// block laid before it, where that line does not end a paragraph. A
/// float's or an inline-block's lines between them, laid after the line
/// holding it, do not part them (`Laid::joins`).
pub fn joined_before(laid: &Laid, at: usize) -> Option<usize> {
    if let Some((before, _)) = laid.joins(at) {
        return before;
    }
    let lines = laid.lines();
    let before = at.checked_sub(1)?;
    let line = lines.get(before)?;
    (!ends_paragraph(line) && line.block() == lines.get(at)?.block()).then_some(before)
}

/// The line after `at` in its paragraph, if any (`joined_before`).
pub fn joined_after(laid: &Laid, at: usize) -> Option<usize> {
    if let Some((_, after)) = laid.joins(at) {
        return after;
    }
    let lines = laid.lines();
    let line = lines.get(at)?;
    let next = lines.get(at + 1)?;
    (!ends_paragraph(line) && next.block() == line.block()).then_some(at + 1)
}

/// The first and last lines of the paragraph line `at` lies in.
pub fn paragraph(laid: &Laid, at: usize) -> (usize, usize) {
    let mut first = at;
    while let Some(before) = joined_before(laid, first) {
        first = before;
    }
    let mut last = at;
    while let Some(after) = joined_after(laid, last) {
        last = after;
    }
    (first, last.min(laid.lines().len().saturating_sub(1)))
}

/// Each line's enclosing line and its paragraph's lines before and after
/// it (`nesting`).
pub(crate) type Nesting = (BudgetVec<u32>, BudgetVec<(u32, u32)>);

/// Where a walk meeting line `line` in tree order starts: the first line
/// of the paragraph of the outermost line it lies in.
fn walk_start(laid: &Laid, line: usize) -> usize {
    paragraph(laid, outermost(laid, line)).0
}

/// Each line's enclosing line (`Laid::outer`), `NONE` for none: lines are
/// in the tree order of their starts, and the lines of a float met within
/// a line, or of an inline-block in it, follow it, starting and ending
/// within it. A line with no item (a `<br>`'s, or one holding only a
/// float) lies in the line holding its block's box, none for a box no
/// line holds. Each line is set once.
/// And each line's paragraph neighbours (`Laid::joins`): a line not
/// ending its paragraph waits for its block's next line, past the lines
/// of the floats and inline-blocks it holds, which wait above it.
pub(crate) fn nesting(laid: &Laid, memory: &Budget) -> Result<Nesting, BudgetError> {
    let lines = laid.lines();
    let mut outer = td_alloc::BudgetVec::with_capacity(memory, lines.len())?;
    let mut joins = BudgetVec::filled(memory, lines.len(), (NONE, NONE))?;
    // The lines that may hold the lines after, each within the one
    // before it, with their first and last places.
    let mut open: BudgetVec<(u32, Place, Place)> = BudgetVec::new(memory)?;
    // Lines whose paragraph goes on, by their block, innermost last.
    let mut waiting_for: BudgetVec<(u32, u32)> = BudgetVec::new(memory)?;
    for (at, line) in lines.iter().enumerate() {
        let index = u32::try_from(at).unwrap_or(NONE);
        let block = line
            .block()
            .and_then(|b| u32::try_from(b.index()).ok())
            .unwrap_or(NONE);
        if let Some(found) = waiting_for.iter().rposition(|&(b, _)| b == block) {
            if let Some(&(_, before)) = waiting_for.get(found) {
                if let Some(slot) = joins.get_mut(before as usize) {
                    slot.1 = index;
                }
                if let Some(slot) = joins.get_mut(at) {
                    slot.0 = before;
                }
            }
            waiting_for.truncate(found);
        }
        if !ends_paragraph(line) {
            waiting_for.try_push((block, index))?;
        }
        let Some((start, end)) = bounds(laid, line) else {
            // The innermost line holding its block's box: those holding a
            // place are the first of the chain.
            let within = box_start(laid, line).map_or(NONE, |place| {
                let held = open.partition_point(|&(_, start, end)| start <= place && place < end);
                held.checked_sub(1)
                    .and_then(|top| open.get(top))
                    .map_or(NONE, |&(line, _, _)| line)
            });
            outer.try_push(within)?;
            continue;
        };
        while open.last().is_some_and(|&(_, _, end)| end <= start) {
            open.pop();
        }
        let within = open.last().map_or(NONE, |&(line, _, _)| line);
        outer.try_push(within)?;
        open.try_push((index, start, end))?;
    }
    Ok((outer, joins))
}

/// The place before the box that laid line `line`, whose places all
/// follow it: where a line with no item lies.
fn box_start(laid: &Laid, line: &Line) -> Option<Place> {
    let node = laid.styled().node_of(line.block()?.index())?.node;
    Some(Place::new(laid, node, 0))
}

/// The float or inline-block (an inline table included) box `index` is,
/// if any, and whether it floats, as layout floats it (a table never
/// does); with it, its parent's index.
fn own_box(laid: &Laid, index: usize) -> (Option<(usize, bool)>, Option<usize>) {
    let styled = laid.styled();
    let Some(node) = styled.node_of(index) else {
        return (None, None);
    };
    let parent = node.parent.map(|parent| parent.index());
    if node.kind != BoxKind::Block {
        return (None, parent);
    }
    if styled.styled().box_floats(index) {
        (Some((index, true)), parent)
    } else if styled.styled().box_is_inline_block(index) {
        (Some((index, false)), parent)
    } else {
        (None, parent)
    }
}

/// The float or inline-block whose lines are those of the boxes from
/// `index` up, the first of them, and whether it floats; none for the
/// page's. A float met before anything on a line lays its lines before
/// that line, and one met after its last piece after it, as paragraphs
/// of the lines around them (`Walker::range`).
fn nested_box(laid: &Laid, mut index: Option<usize>) -> Option<(usize, bool)> {
    while let Some(at) = index {
        #[cfg(test)]
        tests::visit();
        let (own, parent) = own_box(laid, at);
        if own.is_some() {
            return own;
        }
        index = parent;
    }
    None
}

/// Whether line `line` holds a unit of a walk: shown text or a picture,
/// not an inline-block, whose lines are their own.
fn holds_unit(laid: &Laid, line: &Line) -> bool {
    items(laid, line).any(|item| {
        let style = match item.piece {
            Piece::Text(i) => line.fragments.get(i).map(|f| f.style),
            Piece::Replaced(i) => line.replaced.get(i).filter(|r| !r.atomic).map(|r| r.style),
        };
        style.is_some_and(|style| !hidden(laid, style))
    })
}

/// Where a line lies among the pieces of the line holding it: its first
/// piece's start, or, with no piece, its box's.
fn line_start(laid: &Laid, line: &Line) -> Option<Place> {
    items(laid, line)
        .next()
        .map(|item| item.start)
        .or_else(|| box_start(laid, line))
}

/// The line holding `place`, or the last line starting before it; the
/// first line for a place before them all. Lines are in the tree order
/// of their starts, an item-less one taking the next line's start, and
/// item-less lines that end the page none: a place is never on them. A
/// place past a float's lines, within the line it was met in, is that
/// line's.
pub fn line_of(laid: &Laid, place: Place) -> usize {
    let lines = laid.lines();
    let mut at = starting_past(laid, place, true).saturating_sub(1);
    // The line with an item after the one climbed from: a sibling of `at`
    // where its float's lines go on. One with no item takes its start
    // (`starting_past`).
    let from = at;
    let next = || {
        lines
            .iter()
            .enumerate()
            .skip(from + 1)
            .find(|(_, line)| {
                #[cfg(test)]
                tests::visit();
                items(laid, line).next().is_some()
            })
            .map_or(lines.len(), |(next, _)| next)
    };
    // Found once, where a climb needs it.
    let mut found = None;
    let end = |line: usize| {
        lines
            .get(line)
            .and_then(|l| bounds(laid, l))
            .map(|(_, e)| e)
    };
    // Past a float's line, the place lies in the line its lines lie
    // within where that line holds it (between two floats met in it);
    // past the last of them, in that line or after it; past another,
    // between two of them.
    while let Some(outer) = laid.outer(at) {
        let past = end(at).is_some_and(|e| place > e);
        if !past {
            break;
        }
        let held = lines.get(outer).is_some_and(|line| {
            items(laid, line).any(|item| item.start <= place && place <= item.end)
        });
        if !held && laid.outer(*found.get_or_insert_with(next)) == laid.outer(at) {
            break;
        }
        at = outer;
    }
    at
}

/// The first line starting past `place`, or at it unless `at` is set
/// (`lines().len()` for none): lines are in the order of their starts,
/// an item-less one taking the next line's.
fn starting_past(laid: &Laid, place: Place, at: bool) -> usize {
    let lines = laid.lines();
    let start = |line: usize| -> Option<Place> {
        lines
            .get(line..)?
            .iter()
            .find_map(|line| items(laid, line).next().map(|item| item.start))
    };
    let (mut low, mut high) = (0usize, lines.len());
    while low < high {
        let middle = low + (high - low) / 2;
        if start(middle).is_some_and(|start| start < place || (at && start == place)) {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

/// The outermost line `line` lies within, or itself.
pub fn outermost(laid: &Laid, mut line: usize) -> usize {
    while let Some(outer) = laid.outer(line) {
        line = outer;
    }
    line
}

/// Whether a style preserves white space.
fn preserves(laid: &Laid, f: &TextFragment) -> bool {
    matches!(
        laid.styled()
            .style(f.style)
            .and_then(|s| s.get(Property::WhiteSpace)),
        Some(ComputedValue::Keyword("pre" | "pre-wrap" | "break-spaces"))
    )
}

/// What the text between two lines is, after a `Wrap`: the source between
/// the last piece of one and the first of the next, a space where spaces
/// collapse, the run where they are kept, none at an edge. Beside a
/// replaced piece (an image, an inline-block's placeholder), the text
/// node's on the other side.
fn gap<'a>(laid: &'a Laid, a: &Line, b: &Line) -> Option<(&'a str, Place, Place)> {
    let last = shown_items(laid, a).last()?;
    let next = shown_items(laid, b).next()?;
    let fa = match last.piece {
        Piece::Text(i) => a.fragments.get(i),
        Piece::Replaced(_) => None,
    };
    let fb = match next.piece {
        Piece::Text(j) => b.fragments.get(j),
        Piece::Replaced(_) => None,
    };
    let document = laid.tree().document();
    let tail = |f: &TextFragment| document.node(f.node)?.text()?.get(f.range.end..);
    let head = |f: &TextFragment| document.node(f.node)?.text()?.get(..f.range.start);
    let between: &str = match (fa, fb) {
        (Some(fa), Some(fb)) if fa.node == fb.node => {
            tail(fa)?.get(..fb.range.start.checked_sub(fa.range.end)?)?
        }
        (Some(fa), Some(fb)) => match tail(fa)? {
            tail if tail.trim().is_empty() && !tail.is_empty() => tail,
            _ => head(fb)?,
        },
        (Some(fa), None) => tail(fa)?,
        (None, Some(fb)) => head(fb)?,
        (None, None) => return None,
    };
    // Past a cut only what was dropped as space.
    let space = between.trim_start_matches(|c: char| !c.is_whitespace() || c == '\u{a0}');
    if space.is_empty() {
        return None;
    }
    let style = fa.or(fb)?;
    let shown = if preserves(laid, style) { space } else { " " };
    Some((shown, last.end, next.start))
}

/// What a walk over the laid text meets, in tree order.
#[derive(Clone, Copy, Debug)]
pub enum Event<'a> {
    /// A paragraph begins at this line.
    Begin(usize),
    /// A scalar as shown, its places and whether it was laid with no
    /// advance, which only a walk given the measure knows.
    Scalar(char, Place, Place, bool),
    /// A replaced element's `alt` text, its places before and after it.
    Alt(&'a str, Place, Place),
    /// The white space a `Wrap` left between two lines.
    Gap(&'a str, Place, Place),
    /// The lines of floats, or of an inline-block, lying here within the
    /// paragraph walked begin: their paragraphs follow, and then the
    /// paragraph walked goes on. Find matches nothing across them.
    Enter(Nested),
    /// They end, and the walk goes back to the line holding them, where
    /// it does (not past a run of floats' paragraphs among others).
    Leave(Nested, Option<usize>),
    /// The paragraph ends.
    End,
}

/// What lines lie within a paragraph's: those of the floats met where
/// they lie, or those of an inline-block, by its placeholder, which give
/// text in its line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Nested {
    Float,
    Inline,
}

/// Each event from line `from` on in tree order, until `each` breaks;
/// hidden pieces give none. A paragraph's lines are walked in order, a
/// float's or an inline-block's lines where they lie among its pieces
/// (`Event::Enter`), and the float's lines laid after a line, past its
/// last piece, before its next line; a walk from a line within another
/// goes on with the rest of the lines enclosing it. With the layout's
/// measure each scalar's advance is replayed.
pub fn walk<'a>(
    laid: &'a Laid,
    measure: Option<&mut Measure>,
    from: usize,
    each: impl FnMut(Event<'a>) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let top = outermost(laid, from);
    let mut walker = Walker {
        laid,
        measure,
        each,
        from,
        quiet: top != from,
        boxes: [(usize::MAX, None); BOXES],
    };
    walker.range(top, laid.lines().len(), None)
}

/// A walk's state (`walk`). It recurses once for each line enclosing the
/// one walked, which `nesting` bounds by the layout's depth.
struct Walker<'a, 'm, F> {
    laid: &'a Laid,
    measure: Option<&'m mut Measure>,
    each: F,
    from: usize,
    /// Before line `from` is reached, which a walk from within another
    /// line reaches through the lines enclosing it: no events.
    quiet: bool,
    /// Boxes `float_of` climbed through, by `box_slot`, and the float or
    /// inline-block each lies in (`nested_box`), which is the box's alone:
    /// paragraphs share the ancestors they climb to.
    boxes: [(usize, Option<(usize, bool)>); BOXES],
}

/// The slots of `Walker::boxes`; a climb's boxes evict others in theirs.
const BOXES: usize = 128;

/// Box `index`'s slot in `Walker::boxes`, spread so that the boxes of a
/// chain of ancestors fall apart.
fn box_slot(index: usize) -> usize {
    ((index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 57) as usize % BOXES
}

impl<'a, F: FnMut(Event<'a>) -> ControlFlow<()>> Walker<'a, '_, F> {
    /// The float whose lines line `at` is, if no inline-block is nearer.
    fn float_of(&mut self, at: usize) -> Option<usize> {
        let laid = self.laid;
        let block = laid.lines().get(at)?.block()?.index();
        let (own, parent) = own_box(laid, block);
        let found = match own {
            Some(own) => Some(own),
            None => self.box_above(parent),
        };
        found.and_then(|(index, floats)| floats.then_some(index))
    }

    /// `nested_box(from)`, climbing only to a box climbed through before;
    /// the boxes climbed are kept.
    fn box_above(&mut self, from: Option<usize>) -> Option<(usize, bool)> {
        let laid = self.laid;
        let mut index = from;
        let mut stop = None;
        let found = loop {
            let Some(at) = index else { break None };
            stop = Some(at);
            if let Some(&(key, found)) = self.boxes.get(box_slot(at)) {
                if key == at {
                    break found;
                }
            }
            #[cfg(test)]
            tests::visit();
            let (own, parent) = own_box(laid, at);
            if own.is_some() {
                break own;
            }
            index = parent;
        };
        let mut index = from;
        while let Some(at) = index {
            #[cfg(test)]
            tests::visit();
            if let Some(slot) = self.boxes.get_mut(box_slot(at)) {
                *slot = (at, found);
            }
            if Some(at) == stop {
                break;
            }
            index = laid
                .styled()
                .node_of(at)
                .and_then(|node| node.parent)
                .map(|parent| parent.index());
        }
        found
    }

    /// The first line from `at + 1` not within line `at`, at most `hi`:
    /// the lines within a line follow it as one run (`nesting`).
    fn past(&self, at: usize, hi: usize) -> usize {
        let mut k = at + 1;
        while k < hi && self.laid.outer(k).is_some_and(|outer| outer >= at) {
            #[cfg(test)]
            tests::visit();
            k += 1;
        }
        k
    }

    /// The first line from `k` on lying where no line before `limit`
    /// does, of lines `k..hi` within one line: its lines run from `k`
    /// until one starts at or past `limit` (`line_start`), each looked at
    /// once.
    fn before(&self, mut k: usize, hi: usize, limit: Place) -> usize {
        let lines = self.laid.lines();
        while k < hi {
            #[cfg(test)]
            tests::visit();
            let start = lines.get(k).and_then(|line| line_start(self.laid, line));
            if start.is_none_or(|start| start >= limit) {
                break;
            }
            k = self.past(k, hi);
        }
        k
    }

    /// White space `space`, owed before a piece that lines `lo..hi` lie
    /// ahead of: emitted before them where it lies before them; whether
    /// it lies past them, to go after them, or else, among them, none.
    fn gap_ahead(
        &mut self,
        lo: usize,
        hi: usize,
        (text, start, end): (&'a str, Place, Place),
    ) -> ControlFlow<(), bool> {
        let mut first: Option<Place> = None;
        let mut last: Option<Place> = None;
        for line in self.laid.lines().get(lo..hi).unwrap_or(&[]) {
            if let Some((s, e)) = bounds(self.laid, line) {
                first = Some(first.map_or(s, |f| f.min(s)));
                last = Some(last.map_or(e, |l| l.max(e)));
            }
        }
        if first.is_none_or(|first| end <= first) {
            (self.each)(Event::Gap(text, start, end))?;
            return ControlFlow::Continue(false);
        }
        ControlFlow::Continue(last.is_some_and(|last| start >= last))
    }

    /// The float whose lines a walk of floats' lines from line `at` walks
    /// first, entered already.
    fn owner(&self, at: usize) -> Option<usize> {
        if !self.laid.styled().styled().has_floats() {
            return None;
        }
        let line = self.laid.lines().get(at)?;
        nested_box(self.laid, line.block().map(|b| b.index())).map(|(index, _)| index)
    }

    /// Lines `lo..hi`, which lie in one line or none, as paragraphs, those
    /// of float `owner` entered already.
    fn range(&mut self, lo: usize, hi: usize, owner: Option<usize>) -> ControlFlow<()> {
        let floats = self.laid.styled().styled().has_floats();
        // The float whose run of paragraphs is entered: a float's text met
        // as paragraphs of another's is a float's still, entered once.
        let mut entered: Option<usize> = None;
        let mut at = lo;
        while at < hi {
            if floats && !self.quiet {
                let float = self.float_of(at).filter(|&float| Some(float) != owner);
                if float != entered {
                    if entered.is_some() {
                        (self.each)(Event::Leave(Nested::Float, None))?;
                    }
                    if float.is_some() {
                        (self.each)(Event::Enter(Nested::Float))?;
                    }
                    entered = float;
                }
            }
            if !self.quiet {
                (self.each)(Event::Begin(at))?;
            }
            // The line whose `Wrap` leaves white space before the next.
            let mut wrapped: Option<usize> = None;
            loop {
                let past = self.past(at, hi);
                let laid_any = self.line(at, past, wrapped)?;
                if laid_any {
                    wrapped = None;
                }
                match self.laid.lines().get(at).map(Line::end) {
                    Some(LineEnd::Wrap) if laid_any => wrapped = Some(at),
                    Some(LineEnd::Wrap) => {}
                    _ => wrapped = None,
                }
                let next = joined_after(self.laid, at).filter(|&next| next >= past && next < hi);
                let Some(next) = next else {
                    if !self.quiet {
                        (self.each)(Event::End)?;
                    }
                    at = past;
                    break;
                };
                // A float's lines laid past this line's last piece lie
                // before its next line, as they do in tree order, and the
                // white space of a wrap before them where it lies.
                if next > past {
                    let lines = self.laid.lines();
                    let space = match (wrapped, lines.get(at), lines.get(next)) {
                        (Some(_), Some(line), Some(after))
                            if !self.quiet && shown_items(self.laid, after).next().is_some() =>
                        {
                            gap(self.laid, line, after)
                        }
                        _ => None,
                    };
                    if let Some(space) = space {
                        if !self.gap_ahead(past, next, space)? {
                            wrapped = None;
                        }
                    } else {
                        wrapped = None;
                    }
                    self.nested(past, next, Nested::Float, at)?;
                }
                at = next;
            }
        }
        if entered.is_some() {
            (self.each)(Event::Leave(Nested::Float, None))?;
        }
        ControlFlow::Continue(())
    }

    /// Lines `lo..hi` within line `holder`, of `kind`; while quiet, only
    /// where they hold line `from`.
    fn nested(&mut self, lo: usize, hi: usize, kind: Nested, holder: usize) -> ControlFlow<()> {
        if self.quiet && !(lo..hi).contains(&self.from) {
            return ControlFlow::Continue(());
        }
        if !self.quiet {
            (self.each)(Event::Enter(kind))?;
        }
        let owner = match kind {
            Nested::Float => self.owner(lo),
            Nested::Inline => None,
        };
        self.range(lo, hi, owner)?;
        if self.quiet {
            return ControlFlow::Continue(());
        }
        (self.each)(Event::Leave(kind, Some(holder)))
    }

    /// Line `at`'s pieces, the white space after line `wrapped` before
    /// them, with the lines within it, `at + 1..past`, where they lie;
    /// whether it laid any piece shown.
    fn line(&mut self, at: usize, past: usize, wrapped: Option<usize>) -> ControlFlow<(), bool> {
        let laid = self.laid;
        let lines = laid.lines();
        let Some(line) = lines.get(at) else {
            return ControlFlow::Continue(false);
        };
        let mut wrapped = wrapped;
        if self.quiet && at == self.from {
            self.quiet = false;
            wrapped = None;
            (self.each)(Event::Begin(at))?;
        }
        let laid_any = shown_items(laid, line).next().is_some();
        // The white space after line `wrapped`, before the first piece
        // shown, or before or after lines lying ahead of it, where tree
        // order puts it.
        let mut space = match wrapped.and_then(|before| lines.get(before)) {
            Some(before) if laid_any && !self.quiet => gap(laid, before, line),
            _ => None,
        };
        // The next line within it not walked.
        let mut inner = at + 1;
        for item in items(laid, line) {
            let floats = self.before(inner, past, item.start);
            if floats > inner {
                if let Some(gap) = space {
                    if !self.gap_ahead(inner, floats, gap)? {
                        space = None;
                    }
                }
                self.nested(inner, floats, Nested::Float, at)?;
                inner = floats;
            }
            let (atomic, style) = match item.piece {
                Piece::Replaced(i) => line
                    .replaced
                    .get(i)
                    .map_or((false, None), |r| (r.atomic, Some(r.style))),
                Piece::Text(i) => (false, line.fragments.get(i).map(|f| f.style)),
            };
            let shown = style.is_some_and(|style| !hidden(laid, style));
            if shown {
                if let Some((text, start, end)) = space.take() {
                    (self.each)(Event::Gap(text, start, end))?;
                }
            }
            if atomic {
                // Its lines, laid or not, shown or not: none matches
                // across it, and its text lies in this line.
                let own = self.before(inner, past, item.end);
                if self.quiet {
                    if own > inner {
                        self.nested(inner, own, Nested::Inline, at)?;
                    }
                } else {
                    (self.each)(Event::Enter(Nested::Inline))?;
                    self.range(inner, own, None)?;
                    (self.each)(Event::Leave(Nested::Inline, Some(at)))?;
                }
                if own > inner {
                    space = None;
                }
                inner = own;
                continue;
            }
            if self.quiet || !shown {
                continue;
            }
            self.piece(line, item)?;
        }
        if inner < past {
            self.nested(inner, past, Nested::Float, at)?;
        }
        ControlFlow::Continue(laid_any)
    }

    /// A text or replaced piece's events.
    fn piece(&mut self, line: &'a Line, item: Item) -> ControlFlow<()> {
        #[cfg(test)]
        tests::piece();
        let laid = self.laid;
        match item.piece {
            Piece::Text(i) => {
                let Some(f) = line.fragments.get(i) else {
                    return ControlFlow::Continue(());
                };
                if f.display == DisplayText::Space {
                    return (self.each)(Event::Scalar(' ', item.start, item.end, false));
                }
                if let Some(measure) = self.measure.as_deref_mut() {
                    let mut flow = ControlFlow::Continue(());
                    let mut reported = false;
                    let each = &mut self.each;
                    extents(laid, measure, line, f, |e| {
                        reported = true;
                        if flow.is_continue() {
                            flow = each(Event::Scalar(e.shown, e.start, e.end, e.width == 0.0));
                        }
                    });
                    flow?;
                    // A replay refused before any scalar gives them
                    // unmeasured.
                    if reported {
                        return ControlFlow::Continue(());
                    }
                }
                for (offset, c) in fragment_text(laid, f).char_indices() {
                    let shown = match f.display {
                        DisplayText::Replacement => '\u{fffd}',
                        _ => c,
                    };
                    let at = f.range.start + offset;
                    let start = Place::new(laid, f.node, at);
                    let end = Place::new(laid, f.node, at + c.len_utf8());
                    (self.each)(Event::Scalar(shown, start, end, false))?;
                }
                ControlFlow::Continue(())
            }
            Piece::Replaced(i) => {
                let Some(r) = line.replaced.get(i) else {
                    return ControlFlow::Continue(());
                };
                let alt = laid.tree().alt(r.node).unwrap_or("");
                (self.each)(Event::Alt(alt, item.start, item.end))
            }
        }
    }
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Moves of a place by scalar, word and paragraph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Move {
    Left,
    Right,
    WordLeft,
    WordRight,
    Up,
    Down,
}

/// Whether scalar `c`, laid with no advance (`flat`), is a mark that
/// joins the scalar before it: no place falls between them.
fn joins(c: char, flat: bool) -> bool {
    flat && !c.is_whitespace() && !c.is_control()
}

/// Each unit a caret steps over from line `from` on, in tree order, with
/// whether a paragraph ended or was entered or left for a float's or an
/// inline-block's since the unit before it: a scalar with the marks of
/// no advance that follow it, an image whole, the white space a wrap
/// left.
fn units(
    laid: &Laid,
    measure: &mut Measure,
    from: usize,
    mut each: impl FnMut(char, Place, Place, bool) -> ControlFlow<()>,
) {
    let mut ended = false;
    // The unit gathered so far: its scalar, places, whether a paragraph
    // ended before it and whether it is a scalar, which a mark may join.
    let mut pending: Option<(char, Place, Place, bool, bool)> = None;
    let flow = walk(laid, Some(measure), from, |event| {
        let (c, s, e, flat) = match event {
            Event::Scalar(c, s, e, flat) => (c, s, e, Some(flat)),
            Event::Alt(_, s, e) => ('\u{fffc}', s, e, None),
            Event::Gap(_, s, e) => (' ', s, e, None),
            Event::Begin(_) => return ControlFlow::Continue(()),
            Event::End | Event::Enter(_) | Event::Leave(..) => {
                ended = true;
                return ControlFlow::Continue(());
            }
        };
        let crossed = std::mem::take(&mut ended);
        if let Some(unit) = pending.as_mut() {
            if !crossed && unit.4 && flat.is_some_and(|flat| joins(c, flat)) {
                unit.2 = e;
                return ControlFlow::Continue(());
            }
        }
        let gathered = pending.take();
        pending = Some((c, s, e, crossed, flat.is_some()));
        match gathered {
            Some((c, s, e, crossed, _)) => each(c, s, e, crossed),
            None => ControlFlow::Continue(()),
        }
    });
    if let (ControlFlow::Continue(()), Some((c, s, e, crossed, _))) = (flow, pending) {
        let _ = each(c, s, e, crossed);
    }
}

/// `at` moved by `motion` (td-web/DESIGN.md §8): a scalar, the step from a
/// paragraph's end to the next one's start being one of its own; to the
/// end of the word after or the start of the word before; to the end of
/// this paragraph or the next, the start of this one or the one before.
/// At the page's edges it stays. A scalar's marks of no advance move
/// with it.
pub fn moved(laid: &Laid, measure: &mut Measure, at: Place, motion: Move) -> Place {
    let lines = laid.lines();
    let line = line_of(laid, at);
    let (first, _) = paragraph(laid, line);
    // Where a walk back starts: `at`'s own paragraph, else the paragraph
    // of the line before it, else the last outermost paragraph whose
    // lines, its nested ones included, hold a unit before the one `at`
    // lies in, so the unit before `at` in tree order, and the start of
    // each paragraph after it, are met, each line looked at once. A walk
    // goes on in tree order from where it starts, so a later start that
    // meets a unit before `at` meets the latest.
    let outermost_before = || {
        let mut before = walk_start(laid, first);
        while let Some(line) = before.checked_sub(1) {
            let start = walk_start(laid, line).min(line);
            let held = lines
                .get(start..before)
                .is_some_and(|run| run.iter().any(|line| holds_unit(laid, line)));
            before = start;
            if held {
                break;
            }
        }
        before
    };
    let line_before = first.checked_sub(1).map(|line| paragraph(laid, line).0);
    let mut found = None;
    match motion {
        Move::Right => units(laid, measure, first, |_, s, e, crossed| {
            if e <= at {
                return ControlFlow::Continue(());
            }
            // From a paragraph's end, the next one's start is a step.
            found = Some(if crossed && s > at { s } else { e });
            ControlFlow::Break(())
        }),
        Move::Left => {
            let mut back = |from: usize| {
                let mut found = None;
                let mut previous: Option<(Place, Place)> = None;
                units(laid, measure, from, |_, s, e, crossed| {
                    if s >= at {
                        // At a paragraph's or a box's start, past where
                        // the unit before ends: that end.
                        if crossed && previous.is_some_and(|(_, pe)| pe < at) {
                            found = previous.map(|(_, pe)| pe);
                        }
                        return ControlFlow::Break(());
                    }
                    previous = Some((s, e));
                    found = Some(s);
                    ControlFlow::Continue(())
                });
                found
            };
            found = back(first)
                .or_else(|| line_before.and_then(&mut back))
                .or_else(|| back(outermost_before()));
        }
        Move::WordRight => {
            let mut in_word = false;
            let mut last_end = None;
            units(laid, measure, first, |c, _, e, crossed| {
                if e <= at {
                    return ControlFlow::Continue(());
                }
                if in_word && (!word_char(c) || crossed) {
                    return ControlFlow::Break(());
                }
                last_end = Some(e);
                if word_char(c) {
                    in_word = true;
                    found = Some(e);
                }
                ControlFlow::Continue(())
            });
            if found.is_none() {
                found = last_end;
            }
        }
        Move::WordLeft => {
            let mut back = |from: usize| {
                let mut found = None;
                let mut previous_word = false;
                units(laid, measure, from, |c, s, _, crossed| {
                    if s >= at {
                        return ControlFlow::Break(());
                    }
                    let word = word_char(c);
                    if word && (!previous_word || crossed) {
                        found = Some(s);
                    }
                    previous_word = word;
                    ControlFlow::Continue(())
                });
                found
            };
            found = back(first)
                .or_else(|| line_before.and_then(&mut back))
                .or_else(|| back(outermost_before()));
        }
        Move::Up => {
            let start = paragraph_places(laid, at).map(|(start, _)| start);
            found = match start {
                Some(start) if start < at => Some(start),
                // The latest paragraph start before `at` in tree order: a
                // float's or an inline-block's lies within another's.
                _ => {
                    let back = |from: usize| {
                        let mut latest = None;
                        let mut begun = false;
                        let _ = walk(laid, None, from, |event| {
                            let s = match event {
                                Event::Begin(_) => {
                                    begun = true;
                                    return ControlFlow::Continue(());
                                }
                                Event::Scalar(_, s, _, _)
                                | Event::Alt(_, s, _)
                                | Event::Gap(_, s, _) => s,
                                _ => return ControlFlow::Continue(()),
                            };
                            if s >= at {
                                return ControlFlow::Break(());
                            }
                            if std::mem::take(&mut begun) {
                                latest = Some(s);
                            }
                            ControlFlow::Continue(())
                        });
                        latest
                    };
                    line_before
                        .and_then(back)
                        .or_else(|| back(outermost_before()))
                }
            };
        }
        Move::Down => {
            let end = paragraph_places(laid, at).map(|(_, end)| end);
            found = match end {
                Some(end) if end > at => Some(end),
                // The first paragraph end past `at` in tree order.
                _ => {
                    let mut last = None;
                    let mut after = None;
                    let _ = walk(laid, None, first, |event| {
                        match event {
                            Event::Scalar(_, _, e, _)
                            | Event::Alt(_, _, e)
                            | Event::Gap(_, _, e) => {
                                last = Some(e);
                            }
                            Event::End if last.is_some_and(|e| e > at) => {
                                after = last;
                                return ControlFlow::Break(());
                            }
                            _ => {}
                        }
                        ControlFlow::Continue(())
                    });
                    after
                }
            };
        }
    }
    found.unwrap_or(at)
}

/// The start of the page and its end.
pub fn page_bounds(laid: &Laid) -> (Place, Place) {
    let lines = laid.lines();
    let first = lines.iter().find_map(|line| bounds(laid, line));
    // The last line may be a float's, lying within one that ends later.
    let mut last = lines
        .iter()
        .enumerate()
        .rev()
        .find_map(|(at, line)| bounds(laid, line).map(|(_, end)| (at, end)));
    while let Some((at, end)) = last {
        let Some(outer) = laid.outer(at) else {
            break;
        };
        let reach = lines.get(outer).and_then(|l| bounds(laid, l));
        last = Some((outer, reach.map_or(end, |(_, e)| e.max(end))));
    }
    (
        first.map_or(Place::default(), |(start, _)| start),
        last.map_or(Place::default(), |(_, end)| end),
    )
}

/// The word under `at`, or none off a word: a run of letters, digits
/// and `_`, each with the marks of no advance after it, within a
/// paragraph and not across a float's or an inline-block's lines.
pub fn word(laid: &Laid, measure: &mut Measure, at: Place) -> Option<(Place, Place)> {
    let line = line_of(laid, at);
    let (first, _) = paragraph(laid, line);
    let mut run: Option<(Place, Place)> = None;
    let mut found = None;
    units(laid, measure, first, |c, s, e, crossed| {
        // A run a paragraph or a nested box ends holds its end too.
        if crossed {
            if let Some(run) = run.filter(|run| run.0 <= at && at <= run.1) {
                found = Some(run);
                return ControlFlow::Break(());
            }
            run = None;
        }
        if word_char(c) {
            run = Some(run.map_or((s, e), |(start, _)| (start, e)));
        } else {
            if let Some(run) = run.filter(|run| run.0 <= at && at < run.1) {
                found = Some(run);
                return ControlFlow::Break(());
            }
            run = None;
        }
        // Units are in tree order: none after this holds `at`.
        if s > at && run.is_none_or(|run| run.0 > at) {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    found.or(run.filter(|run| run.0 <= at && at <= run.1))
}

/// The paragraph line `line` lies in, start to end.
pub fn paragraph_places_of_line(laid: &Laid, line: usize) -> Option<(Place, Place)> {
    let (first, _) = paragraph(laid, line);
    let mut places: Option<(Place, Place)> = None;
    let mut at = Some(first);
    while let Some(line) = at {
        if let Some((start, end)) = laid.lines().get(line).and_then(|l| bounds(laid, l)) {
            places = Some(places.map_or((start, end), |(first, _)| (first, end)));
        }
        at = joined_after(laid, line);
    }
    places
}

/// The paragraph `at` lies in, start to end.
pub fn paragraph_places(laid: &Laid, at: Place) -> Option<(Place, Place)> {
    paragraph_places_of_line(laid, line_of(laid, at))
}

/// The table cell, and its row, a line's block lies in: the innermost
/// cell box among its block's ancestors, and the box holding it, its row
/// or, for a row CSS left anonymous, its table's box.
fn cell_of(laid: &Laid, line: &Line) -> Option<(usize, Option<usize>)> {
    let styled = laid.styled();
    let mut at = line.block().map(|b| b.index());
    while let Some(index) = at {
        if styled.table_role(index) == Some(TableRole::Cell) {
            let row = styled
                .node_of(index)
                .and_then(|b| b.parent)
                .map(|p| p.index());
            return Some((index, row));
        }
        at = styled
            .node_of(index)
            .and_then(|b| b.parent)
            .map(|p| p.index());
    }
    None
}

/// Cells in a row between `from` (excluded) and `to` (excluded), by its
/// box children: an empty cell lays no line but still gives its tab.
fn cells_between(laid: &Laid, from: usize, to: usize) -> usize {
    let styled = laid.styled();
    let mut count = 0usize;
    let mut at = styled.node_of(from).and_then(|b| b.next_sibling);
    while let Some(id) = at {
        if id.index() == to {
            return count;
        }
        if styled.table_role(id.index()) == Some(TableRole::Cell) {
            count += 1;
        }
        at = styled.node_of(id.index()).and_then(|b| b.next_sibling);
    }
    0
}

/// What a copy puts between pieces it gathers.
struct Copier<'a> {
    out: &'a mut dyn FnMut(&str) -> ControlFlow<()>,
    /// The separator owed before the next text: none, a space, a tab run
    /// or a newline.
    owed: Option<Owed>,
    started: bool,
    /// Whether the text put last ends in a space.
    spaced: bool,
}

#[derive(Clone, Copy)]
enum Owed {
    Newline,
    /// A float's edge: a newline, unless a cell's tabs are owed.
    Edge,
    Space,
    Tabs(usize),
}

impl Copier<'_> {
    fn put(&mut self, text: &str) -> ControlFlow<()> {
        if self.started {
            match self.owed.take() {
                Some(Owed::Newline | Owed::Edge) => (self.out)("\n")?,
                Some(Owed::Space) => (self.out)(" ")?,
                Some(Owed::Tabs(n)) => {
                    for _ in 0..n {
                        (self.out)("\t")?;
                    }
                }
                None => {}
            }
        }
        self.owed = None;
        self.started = true;
        self.spaced = text.ends_with(' ');
        (self.out)(text)
    }
    /// The copy ends at the next piece: a selection ending at a
    /// paragraph's start holds the newline before it.
    fn close(&mut self, at_start: bool) -> ControlFlow<()> {
        if at_start && self.started && matches!(self.owed, Some(Owed::Newline | Owed::Edge)) {
            (self.out)("\n")?;
        }
        ControlFlow::Break(())
    }
    fn owe(&mut self, owed: Owed) {
        self.owed = Some(match (self.owed, owed) {
            (Some(Owed::Newline), _) | (_, Owed::Newline) => Owed::Newline,
            (Some(Owed::Tabs(a)), Owed::Tabs(b)) => Owed::Tabs(a + b),
            (Some(Owed::Tabs(a)), Owed::Space | Owed::Edge) => Owed::Tabs(a),
            (Some(Owed::Edge), Owed::Space) => Owed::Edge,
            (_, owed) => owed,
        });
    }
}

/// A float's separators, owed only between pieces copied: two pieces
/// have a float's edge between them where either lies in a float the
/// walk left between them, so a float with nothing shown parts nothing.
#[derive(Default)]
struct Floats {
    /// How many floats the walk is in.
    depth: usize,
    /// The fewest it was in since the last piece.
    low: usize,
    /// How many the last piece was in, and its cell.
    last: (usize, Option<usize>),
    /// What a float's paragraph's end owes, held until more of its
    /// text, and how many floats the walk was in.
    end: Option<(Owed, usize)>,
}

impl Floats {
    /// A piece in cell `cell` is copied: what is owed before it. Within
    /// one cell a float's edge is a space, as its paragraphs' ends are.
    fn put(&mut self, copier: &mut Copier, cell: Option<usize>) {
        if let Some((end, _)) = self.end.take() {
            copier.owe(end);
        }
        if self.at_edge() {
            if cell.is_none() || cell != self.last.1 {
                copier.owe(Owed::Edge);
            } else if !copier.spaced {
                copier.owe(Owed::Space);
            }
        }
        self.low = self.depth;
        self.last = (self.depth, cell);
    }

    /// Whether a float's edge lies before the next piece.
    fn at_edge(&self) -> bool {
        self.last.0 > self.low || self.depth > self.low
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
        self.low = self.low.min(self.depth);
        if self.end.is_some_and(|(_, depth)| depth > self.depth) {
            self.end = None;
        }
    }

    /// A paragraph ends owing `owed`: held in a float.
    fn end(&mut self, copier: &mut Copier, owed: Owed) {
        if self.depth == 0 {
            copier.owe(owed);
            return;
        }
        // Held across a float with nothing shown, at the fewest floats.
        self.end = Some(match self.end {
            Some((held, depth)) => (
                if matches!(held, Owed::Newline) {
                    held
                } else {
                    owed
                },
                depth.min(self.depth),
            ),
            None => (owed, self.depth),
        });
    }
}

/// Whether a selection from `start` to `end` spans empty item `item`,
/// which has no place: it lies after `start` and before `end` in tree
/// order.
/// No place lies before the empty items ahead of the page's first place,
/// or after those past its last: only a selection of the whole page, from
/// its first place to its last, holds them.
fn empty_held(
    laid: &Laid,
    item: NodeId,
    (start, top): (Place, bool),
    (end, bottom): (Place, bool),
) -> bool {
    let rank = laid.tree().rank(item);
    (top || start.rank < rank) && (bottom || rank < end.rank)
}

/// Where a paragraph in an inline-block ended since `copy`'s last piece:
/// the first such end's depth and what was owed before it.
type Ended = Option<(usize, Option<Owed>)>;

/// The text between `start` and `end` as a copy takes it, handed to `out`
/// a piece at a time until it breaks.
pub fn copy(
    laid: &Laid,
    start: Place,
    end: Place,
    out: &mut dyn FnMut(&str) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let lines = laid.lines();
    // The scalars painted selected, those starting from `start` to before
    // `end`, in tree order (`walk`): from the outermost line the start's
    // lies in until a piece shown starts at or past the end. A selection
    // ending where a line starts holds the newline before it.
    let to = line_of(laid, end);
    let from = outermost(laid, line_of(laid, start));
    let until = starting_past(laid, end, true);
    let at_start = starting_past(laid, end, false) < until;
    let start_cell = lines.get(from).and_then(|l| cell_of(laid, l));
    let end_cell = lines.get(to).and_then(|l| cell_of(laid, l));
    // Within one cell a selection copies as ordinary paragraphs.
    let tables = start_cell.map(|c| c.0) != end_cell.map(|c| c.0)
        || (start_cell.is_none() && end_cell.is_none());
    let mut copier = Copier {
        out,
        owed: None,
        started: false,
        spaced: false,
    };
    let mut cell: Option<(usize, Option<usize>)> = None;
    let mut buffer = [0u8; 4];
    let markers = &laid.layout.markers;
    let mut marker = 0usize;
    let (first, last) = page_bounds(laid);
    let whole = start <= first && end >= last;
    let (top, bottom) = ((start, whole), (end, whole));
    // How many inline-blocks the walk is in, and, where a paragraph in
    // one ended since the last piece, the first such end's depth and what
    // was owed before it: leaving that inline-block owes that again, as
    // its text lies in its line, and no more.
    let mut depth = 0usize;
    let mut ended: Ended = None;
    let mut floats = Floats::default();
    // The line the last paragraph walked began on, and, where it ended
    // and began before `until` with no piece put since, what was owed
    // before its end: a line starting at the end, its first piece not
    // shown, owes nothing for its own paragraph's end.
    let mut begun = 0usize;
    let mut tail: Option<Option<Owed>> = None;
    // Whether the copy stopped at a piece at or past the end, and at one
    // starting there, before which what is owed is as copying it would
    // put it.
    let mut stopped = false;
    let mut exact = false;
    let mut put = |copier: &mut Copier,
                   text: &str,
                   s: Place,
                   e: Place,
                   (ended, tail): (&mut Ended, &mut Option<Option<Owed>>),
                   floats: &mut Floats,
                   cell: Option<usize>| {
        if s >= end {
            stopped = true;
            if s == end {
                exact = true;
                floats.put(copier, cell);
            }
            return ControlFlow::Break(());
        }
        if s >= start && e <= end {
            *ended = None;
            *tail = None;
            floats.put(copier, cell);
            copier.put(text)?;
        }
        ControlFlow::Continue(())
    };
    let flow = walk(laid, None, from, |event| match event {
        Event::Begin(at) => {
            begun = at;
            if at < until {
                tail = None;
            }
            let Some(line) = lines.get(at) else {
                return ControlFlow::Continue(());
            };
            let here = if tables { cell_of(laid, line) } else { None };
            if here.map(|c| c.0) != cell.map(|c| c.0) && copier.started {
                match (cell, here) {
                    (Some((a, row_a)), Some((b, row_b))) if row_a.is_some() && row_a == row_b => {
                        copier.owe(Owed::Tabs(1 + cells_between(laid, a, b)));
                    }
                    _ => copier.owe(Owed::Newline),
                }
            }
            cell = here;
            // The markers on a paragraph's first line, outer first, when
            // the selection holds its start; markers are in line order.
            let first = bounds(laid, line).map(|(s, _)| s);
            let held = first.is_some_and(|s| s >= start && s < end);
            while let Some(m) = markers.get(marker) {
                match m.line {
                    Some(l) if (l as usize) > at => break,
                    // An empty item after this line's start waits for
                    // the line after it.
                    None if first.is_none_or(|s| laid.tree().rank(m.item) > s.rank) => break,
                    // A marker is put as text is, after what is owed.
                    Some(l) if l as usize == at && held && !hidden(laid, m.style) => {
                        floats.put(&mut copier, cell.map(|c| c.0));
                        // Its text ends in its own space.
                        copier.put(m.text().as_str())?;
                    }
                    // An empty item's, before this line, is a paragraph
                    // of its own when the selection spans its item, after
                    // a newline, which no float's edge or end outweighs.
                    None if empty_held(laid, m.item, top, bottom) && !hidden(laid, m.style) => {
                        if copier.started {
                            copier.owe(Owed::Newline);
                        }
                        copier.put(m.text().as_str().trim_end())?;
                        copier.owe(Owed::Newline);
                    }
                    _ => {}
                }
                marker += 1;
            }
            ControlFlow::Continue(())
        }
        Event::Scalar(c, s, e, _) => put(
            &mut copier,
            c.encode_utf8(&mut buffer),
            s,
            e,
            (&mut ended, &mut tail),
            &mut floats,
            cell.map(|c| c.0),
        ),
        Event::Alt(text, s, e) => put(
            &mut copier,
            text,
            s,
            e,
            (&mut ended, &mut tail),
            &mut floats,
            cell.map(|c| c.0),
        ),
        Event::Gap(text, s, e) => {
            if copier.started {
                put(
                    &mut copier,
                    text,
                    s,
                    e,
                    (&mut ended, &mut tail),
                    &mut floats,
                    cell.map(|c| c.0),
                )
            } else {
                ControlFlow::Continue(())
            }
        }
        // A float's text, block-level, is its own paragraphs, set off
        // from the text around it where it has any; an inline-block's
        // lies in its line.
        Event::Enter(Nested::Float) => {
            floats.depth += 1;
            ControlFlow::Continue(())
        }
        // It is a piece of its line, as a picture is, empty or not.
        Event::Enter(Nested::Inline) => {
            depth += 1;
            ended = None;
            floats.put(&mut copier, cell.map(|c| c.0));
            ControlFlow::Continue(())
        }
        Event::Leave(kind, holder) => {
            // Back in the line holding it, in that line's cell.
            if let (true, Some(line)) = (tables, holder.and_then(|at| lines.get(at))) {
                cell = cell_of(laid, line);
            }
            match kind {
                Nested::Inline => {
                    if let Some((_, owed)) = ended.filter(|&(at, _)| at >= depth) {
                        copier.owed = owed;
                        floats.end = None;
                        ended = None;
                    }
                    depth = depth.saturating_sub(1);
                }
                // Its text ends at an edge, owed before the next piece,
                // in an inline-block as well, whose leaving cancels only
                // its own paragraphs' ends after it.
                Nested::Float => floats.leave(),
            }
            ControlFlow::Continue(())
        }
        Event::End => {
            if begun < until {
                tail = Some(copier.owed);
            }
            if copier.started {
                if depth > 0 && ended.is_none() {
                    ended = Some((depth, copier.owed));
                }
                let owed = if cell.is_some() {
                    Owed::Space
                } else {
                    Owed::Newline
                };
                floats.end(&mut copier, owed);
            }
            ControlFlow::Continue(())
        }
    });
    // The consumer broke.
    if flow.is_break() && !stopped {
        return ControlFlow::Break(());
    }
    // Ended before the page's end; where a line starts, with the newline
    // before it, but for its own paragraph's end.
    if stopped || until < lines.len() || at_start {
        if !exact {
            if let Some((end, _)) = floats.end.take() {
                copier.owe(end);
            }
            if let (true, Some(owed)) = (at_start, tail) {
                copier.owed = owed;
            }
        }
        return copier.close(at_start);
    }
    // Empty items after the page's last line, held to its end.
    for m in markers.get(marker..).unwrap_or(&[]) {
        if m.line.is_none() && empty_held(laid, m.item, top, bottom) && !hidden(laid, m.style) {
            if copier.started {
                copier.owe(Owed::Newline);
            }
            copier.put(m.text().as_str().trim_end())?;
        }
    }
    ControlFlow::Continue(())
}

/// The bytes `copy` takes, and whether they pass `most`.
pub fn measure_copy(laid: &Laid, start: Place, end: Place, most: usize) -> (usize, bool) {
    let mut bytes = 0usize;
    let _ = copy(laid, start, end, &mut |text| {
        bytes = bytes.saturating_add(text.len());
        if bytes > most {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    (bytes, bytes > most)
}

/// `copy` into one string under `memory`, measured first so at most
/// `most` bytes are asked for.
pub fn copied(
    laid: &Laid,
    start: Place,
    end: Place,
    most: usize,
    memory: &Budget,
) -> Result<Option<BudgetString>, td_alloc::BudgetError> {
    let (bytes, past) = measure_copy(laid, start, end, most);
    if past {
        return Ok(None);
    }
    let mut out = td_alloc::BudgetString::with_capacity(memory, bytes)?;
    let mut failed = None;
    let _ = copy(laid, start, end, &mut |text| match out.try_push_str(text) {
        Ok(()) => ControlFlow::Continue(()),
        Err(error) => {
            failed = Some(error);
            ControlFlow::Break(())
        }
    });
    match failed {
        Some(error) => Err(error),
        None => Ok(Some(out)),
    }
}

/// A scalar's extent along its line, from its fragment's x, in CSS px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Extent {
    pub start: Place,
    pub end: Place,
    pub x: f64,
    pub width: f64,
    pub shown: char,
}

/// Each scalar of a text fragment with its extent, replayed through
/// `measure` with the style of the block that laid its line; none when
/// the replay refuses (another measure than the layout's).
pub fn extents(
    laid: &Laid,
    measure: &mut Measure,
    line: &Line,
    f: &TextFragment,
    mut each: impl FnMut(Extent),
) -> bool {
    extents_until(laid, measure, line, f, |extent| {
        each(extent);
        ControlFlow::Continue(())
    })
}

/// `extents`, which stops when `each` breaks (td-layout's
/// `replay_until`): true then too.
pub fn extents_until(
    laid: &Laid,
    measure: &mut Measure,
    line: &Line,
    f: &TextFragment,
    mut each: impl FnMut(Extent) -> ControlFlow<()>,
) -> bool {
    let Some(block) = line
        .block()
        .and_then(|b| laid.styled().node_of(b.index()))
        .map(|b| b.style)
    else {
        return false;
    };
    let unit = f64::from(UNIT);
    replay_until(laid.styled().styled(), measure, f, block, |r| {
        each(Extent {
            start: Place::new(laid, f.node, r.range.start),
            end: Place::new(laid, f.node, r.range.end),
            x: f.x + r.pen as f64 / unit,
            width: r.advance as f64 / unit,
            shown: r.shown,
        })
    })
    .is_ok()
}

/// The line under a point of the page in CSS px: the nearest by height,
/// of those the nearest by x, of equals the deepest (an inline-block's
/// line within the line holding it), then the first. Each run of `CHUNK`
/// lines' span holds their boxes, so it bounds how near any of them is:
/// the nearest span's lines are looked at first, then only those of a
/// span no further.
pub fn line_at(laid: &Laid, x: f64, y: f64) -> Option<usize> {
    let lines = laid.lines();
    let spans = laid.line_spans();
    // How many lines a line lies within.
    let depth = |mut line: usize| {
        let mut depth = 0usize;
        while let Some(outer) = laid.outer(line) {
            depth += 1;
            line = outer;
        }
        depth
    };
    let near = |span: &Span| {
        if y < span.top {
            span.top - y
        } else if y >= span.bottom {
            y - span.bottom
        } else {
            0.0
        }
    };
    let scan = |chunk: usize, best: &mut Option<(f64, f64, usize)>| {
        let from = chunk.saturating_mul(CHUNK);
        for (at, line) in lines.iter().enumerate().skip(from).take(CHUNK) {
            if items(laid, line).next().is_none() {
                continue;
            }
            let dy = if y < line.y {
                line.y - y
            } else if y >= line.y + line.height {
                y - (line.y + line.height) + f64::MIN_POSITIVE
            } else {
                0.0
            };
            let (left, right) = horizontal(line);
            let dx = if x < left {
                left - x
            } else if x > right {
                x - right
            } else {
                0.0
            };
            let nearer = best.is_none_or(|(by, bx, b)| {
                dy.total_cmp(&by)
                    .then(dx.total_cmp(&bx))
                    .then_with(|| depth(b).cmp(&depth(at)))
                    .then(at.cmp(&b))
                    .is_lt()
            });
            if nearer {
                *best = Some((dy, dx, at));
            }
        }
    };
    let (first, _) = spans
        .iter()
        .enumerate()
        .map(|(chunk, span)| (chunk, near(span)))
        .filter(|(_, d)| d.is_finite())
        .min_by(|a, b| a.1.total_cmp(&b.1))?;
    let mut best = None;
    scan(first, &mut best);
    for (chunk, span) in spans.iter().enumerate() {
        if chunk != first && best.is_none_or(|(by, _, _)| near(span) <= by) {
            scan(chunk, &mut best);
        }
    }
    best.map(|(_, _, at)| at)
}

/// A line's pieces' left and right edges.
pub fn horizontal(line: &Line) -> (f64, f64) {
    let texts = line.fragments.iter().map(|f| (f.x, f.x + f.width));
    let images = line.replaced.iter().map(|r| (r.x, r.x + r.width));
    texts
        .chain(images)
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(l, r), (a, b)| {
            (l.min(a), r.max(b))
        })
}

/// The place nearest a point in CSS px: on a line, between the scalars by
/// the halves of their advances; left of it its start, right its end.
/// With the piece under it, if any.
pub fn place_at(
    laid: &Laid,
    measure: &mut Measure,
    x: f64,
    y: f64,
) -> Option<(Place, Option<Item>)> {
    let at = line_at(laid, x, y)?;
    let line = laid.lines().get(at)?;
    let (start, end) = bounds(laid, line)?;
    let (left, right) = horizontal(line);
    // A point between lines falls on the nearest, on no piece of it.
    if y < line.y || y >= line.y + line.height {
        let near = if y < line.y { start } else { end };
        return Some((near, None));
    }
    if x < left {
        return Some((start, None));
    }
    if x >= right {
        return Some((end, None));
    }
    let mut best = (end, None);
    for item in items(laid, line) {
        match item.piece {
            Piece::Text(i) => {
                let Some(f) = line.fragments.get(i) else {
                    continue;
                };
                // A line beginning with marks of no advance still has a
                // start a point reaches.
                if f.width == 0.0 && x <= f.x && item.start == start {
                    return Some((start, Some(item)));
                }
                if x < f.x || x >= f.x + f.width {
                    continue;
                }
                let mut found = item.end;
                let mut done = false;
                // Whether a scalar with an advance came before: a mark of
                // none after it is its, and `settle` keeps them together.
                let mut based = false;
                extents(laid, measure, line, f, |e| {
                    if done {
                        return;
                    }
                    if e.width > 0.0 {
                        based = true;
                        if x < e.x + e.width / 2.0 {
                            found = e.start;
                            done = true;
                        } else if x < e.x + e.width {
                            found = e.end;
                            done = true;
                        }
                    } else if !based && x <= e.x {
                        // A line beginning with marks still has a start.
                        found = e.start;
                        done = true;
                    }
                });
                return Some((settle(laid, measure, at, found), Some(item)));
            }
            Piece::Replaced(i) => {
                let Some(r) = line.replaced.get(i) else {
                    continue;
                };
                if x < r.x || x >= r.x + r.width {
                    continue;
                }
                let place = if x < r.x + r.width / 2.0 {
                    item.start
                } else {
                    item.end
                };
                return Some((place, Some(item)));
            }
        }
    }
    // Between pieces: the next piece's start.
    for item in items(laid, line) {
        let edge = match item.piece {
            Piece::Text(i) => line.fragments.get(i).map(|f| f.x),
            Piece::Replaced(i) => line.replaced.get(i).map(|r| r.x),
        };
        if edge.is_some_and(|edge| x < edge) {
            best = (item.start, None);
            break;
        }
    }
    Some(best)
}

/// `at`, a place on line `line`, moved past the marks of no advance
/// after it when it falls between them and their base.
fn settle(laid: &Laid, measure: &mut Measure, line: usize, at: Place) -> Place {
    let mut settled = at;
    units(laid, measure, line, |_, s, e, _| {
        if s >= at {
            return ControlFlow::Break(());
        }
        if at < e {
            settled = e;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    settled
}

/// Where a place is drawn: its line and the x of its caret, CSS px.
pub fn locate(laid: &Laid, measure: &mut Measure, place: Place) -> Option<(usize, f64)> {
    let at = line_of(laid, place);
    let line = laid.lines().get(at)?;
    let mut x = horizontal(line).0;
    if !x.is_finite() {
        return Some((at, 0.0));
    }
    for item in items(laid, line) {
        if place < item.start {
            break;
        }
        match item.piece {
            Piece::Text(i) => {
                let Some(f) = line.fragments.get(i) else {
                    continue;
                };
                x = f.x + f.width;
                if place < item.end {
                    let mut found = f.x;
                    extents(laid, measure, line, f, |e| {
                        if e.start < place {
                            found = e.x + e.width;
                        }
                    });
                    x = found;
                    break;
                }
            }
            Piece::Replaced(i) => {
                let Some(r) = line.replaced.get(i) else {
                    continue;
                };
                x = if place <= item.start {
                    r.x
                } else {
                    r.x + r.width
                };
            }
        }
    }
    Some((at, x))
}

/// A place for a history entry's anchor in a page parsed anew: kept when
/// its node is a text node holding its offset on a scalar boundary.
pub fn revive(laid: &Laid, place: Place) -> Option<Place> {
    let node = place.node?;
    let text = laid.tree().document().node(node)?.text()?;
    let offset = place.offset as usize;
    if !text.is_char_boundary(offset) || laid.tree().rank(node) != place.rank {
        return None;
    }
    Some(place)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::page::tests::{laid, laid_for, WANT};
    use crate::page::Want;
    use crate::pipeline::tests::measure;
    use crate::pipeline::text_page;
    use crate::tree::Source;
    use std::cell::Cell;

    thread_local! {
        /// Lines a walk looked at to find where nested lines lie.
        static VISITS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn visit() {
        VISITS.with(|visits| visits.set(visits.get() + 1));
    }

    thread_local! {
        /// Pieces a walk gave events for.
        static PIECES: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn piece() {
        PIECES.with(|pieces| pieces.set(pieces.get() + 1));
    }

    /// The pieces `run` gives events for.
    fn pieces(run: impl FnOnce()) -> usize {
        PIECES.with(|pieces| pieces.set(0));
        run();
        PIECES.with(Cell::get)
    }

    /// The lines `run` looks at to find where nested lines lie.
    fn visits(run: impl FnOnce()) -> usize {
        VISITS.with(|visits| visits.set(0));
        run();
        VISITS.with(Cell::get)
    }

    fn all(laid: &Laid) -> String {
        let (start, end) = page_bounds(laid);
        let mut out = String::new();
        let _ = copy(laid, start, end, &mut |text| {
            out.push_str(text);
            ControlFlow::Continue(())
        });
        out
    }

    /// `at` moved by `motion`, measured as the tests lay.
    fn mv(laid: &Laid, at: Place, motion: Move) -> Place {
        moved(laid, &mut measure(WANT), at, motion)
    }

    fn copy_of(laid: &Laid, start: Place, end: Place) -> String {
        let mut out = String::new();
        let _ = copy(laid, start, end, &mut |text| {
            out.push_str(text);
            ControlFlow::Continue(())
        });
        out
    }

    fn narrow(markup: &str) -> Laid {
        // 8 cells a line at 64 px.
        laid_for(
            markup,
            Source::Markup,
            Want {
                width: 80.0,
                ..WANT
            },
        )
    }

    #[test]
    fn paragraphs_end_at_newlines_breaks_and_blocks() {
        let laid = laid("<p>one two</p><p>three<br>four</p><pre>five\nsix</pre>");
        assert_eq!(all(&laid), "one two\nthree\nfour\nfive\nsix");
    }

    #[test]
    fn a_wrap_puts_back_the_space_laid_between_its_lines() {
        let laid = narrow("<body style=margin:0><p>alpha beta gamma <b>delta</b> epsilon");
        assert!(laid.lines().len() >= 3);
        assert_eq!(all(&laid), "alpha beta gamma delta epsilon");
        let pre = narrow("<body style=margin:0><p style=white-space:pre-wrap>aaaaa   bbbbb");
        assert_eq!(all(&pre), "aaaaa   bbbbb");
        // A word split across lines gets nothing between its halves.
        let split = narrow("<body style=margin:0><p>abcdefghijklmnopqrstuvwxyz");
        assert_eq!(all(&split), "abcdefghijklmnopqrstuvwxyz");
    }

    #[test]
    fn tables_copy_as_rows_of_cells_and_one_cell_as_paragraphs() {
        let laid = laid("<table><tr><td>a<br>b<td><td>c</tr><tr><td>d<td>e</table><p>after");
        assert_eq!(all(&laid), "a b\t\tc\nd\te\nafter");
        let (start, _) = page_bounds(&laid);
        // After `a`, the step past its `br` is one of its own.
        let b = mv(
            &laid,
            mv(&laid, mv(&laid, start, Move::Right), Move::Right),
            Move::Right,
        );
        let mut within = String::new();
        let _ = copy(&laid, start, b, &mut |t| {
            within.push_str(t);
            ControlFlow::Continue(())
        });
        assert_eq!(within, "a\nb");
    }

    #[test]
    fn list_markers_and_alt_text_are_copied() {
        let laid = laid("<ol><li>first<li>second</ol><p><img alt=picture>");
        assert_eq!(all(&laid), "1. first\n2. second\npicture");
    }

    /// An empty item first or last on the page is held by a selection
    /// from the page's start or to its end.
    #[test]
    fn empty_items_at_the_page_s_ends_are_copied_whole() {
        let one = laid("<ul><li>one<li></ul>");
        assert_eq!(all(&one), "• one\n•");
        let two = laid("<ol><li><li>two</ol>");
        assert_eq!(all(&two), "1.\n2. two");
        // A selection of part of the page holds neither.
        let copied = |laid: &Laid, from: u32, to: u32| {
            let (start, end) = page_bounds(laid);
            let mut out = String::new();
            let _ = copy(
                laid,
                Place {
                    offset: start.offset + from,
                    ..start
                },
                Place {
                    offset: end.offset - to,
                    ..end
                },
                &mut |piece| {
                    out.push_str(piece);
                    ControlFlow::Continue(())
                },
            );
            out
        };
        assert_eq!(copied(&one, 1, 0), "ne");
        assert_eq!(copied(&two, 0, 1), "2. tw");
    }

    /// A line with no item among a float's or an inline-block's lines lies
    /// within the line holding its box, as its box's other lines do; one
    /// of a box no line holds, as those ending the page, in none.
    #[test]
    fn item_less_lines_lie_where_their_box_does() {
        for markup in [
            "<body style=margin:0><p style=margin:0>foo<span style=float:left><br>QQ</span>bar",
            "<body style=margin:0><p style=margin:0>foo\
             <span style=display:inline-block><br>QQ</span>bar",
        ] {
            let laid = laid(markup);
            let lines = laid.lines();
            let empty = (0..lines.len())
                .find(|&at| bounds(&laid, &lines[at]).is_none())
                .unwrap();
            assert_eq!(laid.outer(empty), Some(0), "{markup}");
            assert_eq!(laid.outer(empty), laid.outer(empty + 1), "{markup}");
        }
        let laid = laid(
            "<body style=margin:0><p style=margin:0>a<span style=float:left>b</span>\
             <pre>\n\n\n</pre>",
        );
        let lines = laid.lines();
        let last = lines.len() - 1;
        assert!(bounds(&laid, &lines[last]).is_none());
        assert_eq!(laid.outer(last), None);
    }

    /// Around floats met within lines, among them two in one line and
    /// one in a paragraph's second line: every laid piece is found on its
    /// line, and a copy takes the scalars painted selected, those starting
    /// from the start to before the end, in tree order.
    #[test]
    fn copies_around_floats_take_what_is_selected() {
        for markup in [
            "<body style=margin:0><p style=margin:0>one <span style=float:right;width:40px>\
             aa bb</span> two <span style=float:right;width:40px>cc dd</span> three",
            "<body style=margin:0><p style=margin:0>aaaa bbbb cccc dddd one two \
             <span style=float:right;width:40px>in the float</span> three four five six \
             seven eight<p style=margin:0>next",
        ] {
            let laid = laid(markup);
            let lines = laid.lines();
            assert!(lines.iter().any(|line| line.block() != lines[0].block()));
            let mut places = Vec::new();
            for (at, line) in lines.iter().enumerate() {
                for item in items(&laid, line) {
                    assert_eq!(line_of(&laid, item.start), at, "{markup} {item:?}");
                    places.push(item.start);
                    places.push(item.end);
                }
            }
            places.sort();
            places.dedup();
            let mut scalars = Vec::new();
            let _ = walk(&laid, None, 0, |event| {
                if let Event::Scalar(c, s, _, _) = event {
                    scalars.push((c, s));
                }
                ControlFlow::Continue(())
            });
            for (i, &from) in places.iter().enumerate() {
                for &to in places.iter().skip(i + 1) {
                    let shown: String = scalars
                        .iter()
                        .filter(|(c, s)| *s >= from && *s < to && !c.is_whitespace())
                        .map(|(c, _)| *c)
                        .collect();
                    let mut out = String::new();
                    let _ = copy(&laid, from, to, &mut |text| {
                        out.push_str(text);
                        ControlFlow::Continue(())
                    });
                    out.retain(|c| !c.is_whitespace());
                    assert_eq!(out, shown, "{markup} {from:?} {to:?}");
                }
            }
        }
    }

    /// Around inline-blocks, one ending the page's last line and one in
    /// another: every laid piece is found on its line, a copy takes the
    /// scalars painted selected in tree order, an inline-block's text in
    /// its line, the page's end is past its text, and its placeholder
    /// gives no `alt`.
    #[test]
    fn places_and_copies_around_inline_blocks() {
        let ib = "style=display:inline-block";
        for markup in [
            format!(
                "<body style=margin:0><p style=margin:0>one <span {ib}>aa bb<br>cc</span> \
                 two three<p style=margin:0>next"
            ),
            format!("<body style=margin:0><p style=margin:0>one two <span {ib}>aa<br>bb</span>"),
            format!(
                "<body style=margin:0><p style=margin:0>x <span {ib}>a <span {ib}>b<br>c</span> \
                 d</span> y"
            ),
        ] {
            let laid = laid(&markup);
            let lines = laid.lines();
            assert!(laid.outer(1) == Some(0), "{markup}");
            let mut places = Vec::new();
            for (at, line) in lines.iter().enumerate() {
                for item in items(&laid, line) {
                    assert_eq!(line_of(&laid, item.start), at, "{markup} {item:?}");
                    places.push(item.start);
                    places.push(item.end);
                }
            }
            places.sort();
            places.dedup();
            let mut scalars = Vec::new();
            let _ = walk(&laid, None, 0, |event| {
                assert!(!matches!(event, Event::Alt(..)), "{markup}: an alt");
                if let Event::Scalar(c, s, _, _) = event {
                    scalars.push((c, s));
                }
                ControlFlow::Continue(())
            });
            for (i, &from) in places.iter().enumerate() {
                for &to in places.iter().skip(i + 1) {
                    let shown: String = scalars
                        .iter()
                        .filter(|(c, s)| *s >= from && *s < to && !c.is_whitespace())
                        .map(|(c, _)| *c)
                        .collect();
                    let mut out = String::new();
                    let _ = copy(&laid, from, to, &mut |text| {
                        out.push_str(text);
                        ControlFlow::Continue(())
                    });
                    out.retain(|c| !c.is_whitespace());
                    assert_eq!(out, shown, "{markup} {from:?} {to:?}");
                }
            }
            let (first, last) = page_bounds(&laid);
            let all = copy_of(&laid, first, last);
            assert!(
                all.split_whitespace().count() >= 4 && !all.contains('\u{fffc}'),
                "{all:?}"
            );
            let every: String = scalars.iter().map(|(c, _)| *c).collect();
            assert_eq!(
                all.split_whitespace().collect::<String>(),
                every.split_whitespace().collect::<String>()
            );
        }
        let laid = laid(&format!(
            "<body style=margin:0><p style=margin:0>one <span {ib}>aa bb<br>cc</span> two"
        ));
        let (first, last) = page_bounds(&laid);
        assert_eq!(copy_of(&laid, first, last), "one aa bb\ncc two");
    }

    /// The places of the first `word` laid.
    fn word_at(laid: &Laid, word: &str) -> (Place, Place) {
        laid.lines()
            .iter()
            .flat_map(|line| line.fragments.iter())
            .find_map(|f| {
                let at = f.range.start + fragment_text(laid, f).find(word)?;
                Some((
                    Place::new(laid, f.node, at),
                    Place::new(laid, f.node, at + word.len()),
                ))
            })
            .unwrap()
    }

    /// A paragraph wrapped around a float's or an inline-block's lines,
    /// laid after its first line, goes on past them: its lines are one
    /// paragraph, copied whole, the box's text where it lies (a float's
    /// as paragraphs of their own, an inline-block's in the line), and
    /// the keys step into the box and out in tree order.
    #[test]
    fn a_paragraph_goes_on_across_a_float_s_or_an_inline_block_s_lines() {
        let p = "<body style=margin:0><p style=margin:0;width:120px>one two ";
        for (markup, all_text, part, inside) in [
            (
                format!("{p}<span style=float:left>FL</span>three four five six seven"),
                "one two \nFL\nthree four five six seven",
                "two \nFL\nthree four",
                "FL",
            ),
            (
                format!(
                    "{p}<span style=display:inline-block>IB<br>x</span> three four five six seven"
                ),
                "one two IB\nx three four five six seven",
                "two IB\nx three four",
                "IB",
            ),
            (
                format!("{p}<span style=display:inline-block>QQ</span>three four five six seven"),
                "one two QQthree four five six seven",
                "two QQthree four",
                "QQ",
            ),
            // A float met past a line's last piece lies before its next.
            (
                format!("{p}three<span style=float:left>FL</span> four five six seven"),
                "one two three\nFL\nfour five six seven",
                "two three\nFL\nfour",
                "FL",
            ),
        ] {
            let laid = laid(&markup);
            let lines = laid.lines();
            let last = lines.len() - 1;
            let holder = lines[0].block();
            assert!(lines.iter().any(|line| line.block() != holder), "{markup}");
            for (at, line) in lines.iter().enumerate() {
                let para = paragraph(&laid, at);
                if line.block() == holder {
                    assert_eq!(para, (0, last), "{markup}: line {at}");
                } else {
                    assert!(para.0 > 0 && para.1 < last, "{markup}: line {at}");
                }
            }
            assert_eq!(all(&laid), all_text, "{markup}");
            let (one, _) = word_at(&laid, "one");
            let (_, seven) = word_at(&laid, "seven");
            assert_eq!(paragraph_places(&laid, one), Some((one, seven)), "{markup}");
            assert_eq!(mv(&laid, one, Move::Down), seven, "{markup}");
            assert_eq!(mv(&laid, seven, Move::Up), one, "{markup}");
            let (two, _) = word_at(&laid, "two");
            let (_, four) = word_at(&laid, "four");
            assert_eq!(copy_of(&laid, two, four), part, "{markup}");
            let mut m = measure(WANT);
            let (three, three_end) = word_at(&laid, "three");
            assert_eq!(
                word(&laid, &mut m, three),
                Some((three, three_end)),
                "{markup}"
            );
            let (four_start, _) = word_at(&laid, "four");
            assert_eq!(
                word(&laid, &mut m, four_start),
                Some((four_start, four)),
                "{markup}"
            );
            // Into the box and out, a step each.
            let (box_start, box_end) = word_at(&laid, inside);
            let mut before = one;
            for _ in 0..64 {
                let right = mv(&laid, before, Move::Right);
                if right == box_start {
                    break;
                }
                before = right;
            }
            assert_eq!(mv(&laid, before, Move::Right), box_start, "{markup}");
            assert_eq!(mv(&laid, box_start, Move::Left), before, "{markup}");
            let after = mv(&laid, box_end, Move::Right);
            assert!(after > box_end, "{markup}");
            assert_eq!(mv(&laid, after, Move::Left), box_end, "{markup}");
        }
    }

    /// The separators of a copy: its text without spaces and the marks a
    /// picture with no `alt` stands for, a float's marks newlines, each
    /// run of newlines one, none at either end.
    fn separators(text: &str) -> String {
        let mut out = String::new();
        let marks = |c| if c == '\u{2}' { '\n' } else { c };
        for c in text
            .chars()
            .filter(|&c| c != ' ' && c != '\u{1}')
            .map(marks)
        {
            if c != '\n' || !out.is_empty() && !out.ends_with('\n') {
                out.push(c);
            }
        }
        out.trim_end_matches('\n').to_string()
    }

    /// Generated wrapped paragraphs holding floats and inline-blocks, some
    /// nested, some empty and some holding a `<br>`, mid-paragraph after
    /// another paragraph: a walk is in tree order, every piece starting
    /// where the one before it ends or after; a copy between any two
    /// places takes exactly the scalars painted selected; the page's copy
    /// has the separators the markup gives, a float's text between
    /// newlines and an inline-block's in its line with a newline only
    /// before its own later paragraphs; the holder's lines are one
    /// paragraph and a walk begins each paragraph once; and each move
    /// from a place goes the way it moves or stays only where nothing
    /// lies that way.
    #[test]
    fn generated_paragraphs_go_on_across_nested_boxes() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % n
        };
        let words = ["ab", "cde", "fghij", "k", "lmno"];
        let boxes = [
            ("<span style=float:left;width:40px>", "</span>", true),
            ("<span style=float:right>", "</span>", true),
            ("<span style=display:inline-block>", "</span>", false),
            ("<a href=/l style=display:inline-block>", "</a>", false),
        ];
        let mut m = measure(WANT);
        let mut pairs = 0usize;
        for page in 0..120 {
            let width = 60 + next(140);
            let mut markup = format!(
                "<body style=margin:0><p style=margin:0>lead<p style=margin:0;width:{width}px>"
            );
            // The open boxes, each with its close, whether it floats and
            // the text a copy of it gives.
            let mut open: Vec<(&str, bool, String)> = vec![("", false, String::new())];
            for _ in 0..16 + next(24) {
                let depth = open.len() - 1;
                match next(10) {
                    0 | 1 if depth < 3 => {
                        // No link in a link, which the parser would part.
                        let (start, close, floats) = boxes[next(4) as usize];
                        if close == "</a>" && open.iter().any(|o| o.0 == "</a>") {
                            continue;
                        }
                        markup.push_str(start);
                        open.push((close, floats, String::new()));
                    }
                    2 if depth > 0 => {
                        let (close, floats, text) = open.pop().unwrap();
                        markup.push_str(close);
                        // An inline-block is a piece, its last paragraph's
                        // end owing nothing; a float's text lies between
                        // newlines, marked apart from its paragraphs' ends,
                        // as an inline-block holding it does not cancel
                        // them.
                        let copied = if !floats {
                            format!("\u{1}{}", text.trim_end_matches('\n'))
                        } else if text.chars().all(|c| c == '\n') {
                            // Nothing shown, no edge.
                            String::new()
                        } else {
                            format!("\u{2}{text}\u{2}")
                        };
                        open.last_mut().unwrap().2.push_str(&copied);
                    }
                    3 if depth > 0 => {
                        markup.push_str("<br>");
                        open.last_mut().unwrap().2.push('\n');
                    }
                    4 => {
                        markup.push_str("<img width=12 height=8>");
                        open.last_mut().unwrap().2.push('\u{1}');
                    }
                    _ => {
                        let word = words[next(5) as usize];
                        markup.push_str(word);
                        markup.push(' ');
                        open.last_mut().unwrap().2.push_str(word);
                    }
                }
            }
            while open.len() > 1 {
                let (close, floats, text) = open.pop().unwrap();
                markup.push_str(close);
                let copied = if !floats {
                    format!("\u{1}{}", text.trim_end_matches('\n'))
                } else if text.chars().all(|c| c == '\n') {
                    String::new()
                } else {
                    format!("\u{2}{text}\u{2}")
                };
                open.last_mut().unwrap().2.push_str(&copied);
            }
            markup.push_str("tail words end");
            let expected = separators(&format!("lead\n{}tailwordsend", open[0].2));
            let laid = laid(&markup);
            let lines = laid.lines();
            let mut scalars = Vec::new();
            let mut spans: Vec<(Place, Place)> = Vec::new();
            let mut begun = 0usize;
            let _ = walk(&laid, None, 0, |event| {
                match event {
                    Event::Scalar(c, s, e, _) => {
                        scalars.push((c, s));
                        spans.push((s, e));
                    }
                    Event::Alt(_, s, e) | Event::Gap(_, s, e) => spans.push((s, e)),
                    Event::Begin(_) => begun += 1,
                    _ => {}
                }
                ControlFlow::Continue(())
            });
            assert!(
                spans
                    .windows(2)
                    .all(|w| w[0].0 < w[1].0 && w[0].1 <= w[1].0),
                "{page}: {markup}: not in tree order"
            );
            let (first, last) = page_bounds(&laid);
            assert_eq!(
                separators(&copy_of(&laid, first, last)),
                expected,
                "{page}: {markup}"
            );
            // The text after the boxes is the holder's: a float first in it
            // lays its lines before its first, a box last its own after.
            let tail = line_of(&laid, word_at(&laid, "tail").0);
            let holder = lines[tail].block();
            let whole = paragraph(&laid, tail);
            let mut firsts: Vec<usize> = Vec::new();
            for (at, line) in lines.iter().enumerate() {
                let para = paragraph(&laid, at);
                if line.block() == holder {
                    assert_eq!(para, whole, "{page}: {markup}: line {at}");
                }
                if !firsts.contains(&para.0) {
                    firsts.push(para.0);
                }
            }
            assert_eq!(begun, firsts.len(), "{page}: {markup}");
            let mut places = Vec::new();
            for line in lines {
                for item in items(&laid, line) {
                    places.push(item.start);
                    places.push(item.end);
                }
            }
            places.sort();
            places.dedup();
            for _ in 0..60 {
                let a = places[next(places.len() as u64) as usize];
                let b = places[next(places.len() as u64) as usize];
                let (from, to) = (a.min(b), a.max(b));
                let shown: String = scalars
                    .iter()
                    .filter(|(c, s)| *s >= from && *s < to && !c.is_whitespace())
                    .map(|(c, _)| *c)
                    .collect();
                let mut out = copy_of(&laid, from, to);
                out.retain(|c| !c.is_whitespace());
                assert_eq!(out, shown, "{page}: {markup} {from:?} {to:?}");
                pairs += 1;
            }
            for _ in 0..12 {
                let at = places[next(places.len() as u64) as usize];
                let ahead = spans.iter().any(|&(_, e)| e > at);
                let behind = spans.iter().any(|&(s, _)| s < at);
                for motion in [Move::Right, Move::WordRight, Move::Down] {
                    let to = moved(&laid, &mut m, at, motion);
                    assert!(
                        to > at || to == at && (motion != Move::Right || !ahead),
                        "{page}: {markup}: {motion:?} from {at:?} to {to:?}"
                    );
                }
                for motion in [Move::Left, Move::WordLeft, Move::Up] {
                    let to = moved(&laid, &mut m, at, motion);
                    assert!(
                        to < at || to == at && (motion != Move::Left || !behind),
                        "{page}: {markup}: {motion:?} from {at:?} to {to:?}"
                    );
                }
            }
        }
        assert!(pairs > 5_000);
    }

    /// The text a page's matches of `query` copy.
    fn found_in(laid: &Laid, query: &str) -> Vec<String> {
        let page = td_alloc::Budget::new(64 << 20).unwrap();
        let hits = crate::search::find(laid, query, &page).unwrap();
        hits.iter()
            .map(|hit| copy_of(laid, hit.start, hit.end))
            .collect()
    }

    /// An inline-block's leaving cancels only the newlines its own
    /// paragraphs' ends owe after its start: the separator before it is
    /// kept, empty or nested as it may be, as is a `<br>`'s before an
    /// empty one and a float's newline after its text.
    #[test]
    fn an_inline_block_keeps_the_separators_around_it() {
        let ib = "<span style=display:inline-block;width:16px;height:16px></span>";
        for (markup, text) in [
            (format!("<p>prev</p><p>{ib}two</p>"), "prev\ntwo"),
            (
                format!(
                    "<table><tr><td>a</td><td>{ib}b</td></tr><tr><td>\
                     <span style=display:inline-block;width:8px;height:8px></span>c</td></tr></table>"
                ),
                "a\tb\nc",
            ),
            (
                "<p>prev</p><p><span style=display:inline-block>\
                 <span style=display:inline-block></span></span>two</p>"
                    .to_string(),
                "prev\ntwo",
            ),
            (
                "<p>one <span style=display:inline-block>aa<br>bb</span> two".to_string(),
                "one aa\nbb two",
            ),
            (
                format!("<p>one <span style=display:inline-block>aa<br>{ib}</span>bb"),
                "one aa\nbb",
            ),
            (
                "<p>one <span style=display:inline-block>aa \
                 <span style=float:right>fl</span></span>tail"
                    .to_string(),
                "one aa\nfl\ntail",
            ),
            (
                "<p>one <span style=display:inline-block><span style=float:right>fl</span>\
                 aa</span> tail"
                    .to_string(),
                "one \nfl\naa tail",
            ),
            (
                "<p style=margin:0;width:196px>k <span style=float:right>fg \
                 <a href=/l style=display:inline-block><span style=float:right>fghij lmno \
                 lmno lmno ab cde k cde ab fghij fghij k ab </span>hi \
                 <span style=display:inline-block>cde </span></a></span>tail"
                    .to_string(),
                "k \nfg \nfghij lmno lmno lmno ab cde k cde ab fghij fghij k ab\nhi cde\ntail",
            ),
        ] {
            assert_eq!(all(&laid(&markup)), text, "{markup}");
        }
    }

    /// An inline table's cells copy as a table's in an inline-block do:
    /// tabs between a row's cells, newlines between its rows, its text
    /// its own paragraphs within the line holding it; every piece starts
    /// on its own line, and a copy between any two places holds what is
    /// shown between them.
    #[test]
    fn an_inline_table_copies_as_its_cells() {
        let it = "style=display:inline-table";
        let ib = "style=display:inline-block";
        for (table, text) in [
            (
                "<table {}><tr><td>a<td>b<tr><td>c<td>d</table>",
                "one \na\tb\nc\td two",
            ),
            (
                "<table {}><caption>cap</caption><tr><td>a bb<br>cc<td>d</table>",
                "one cap\na bb cc\td two",
            ),
            ("<table {}><tr><td>a<td><td>b</table>", "one \na\t\tb two"),
            (
                "<table {}><tr><td>a <span style=display:inline-block>bb</span> c\
                 <td>d</table>",
                "one \na bb c\td two",
            ),
            ("<table {}></table>", "one  two"),
            // A caption alone, its text the paragraph's.
            ("<table {}><caption>cde</caption></table>", "one cde two"),
            (
                "<table {}><caption style=caption-side:bottom>cde</caption></table>",
                "one cde two",
            ),
        ] {
            let inline = format!("<p>one {} two", table.replace("{}", it));
            let boxed = format!("<p>one <span {ib}>{}</span> two", table.replace("{}", ""));
            let laid = laid(&inline);
            assert_eq!(all(&laid), text, "{inline}");
            assert_eq!(all(&crate::page::tests::laid(&boxed)), text, "{boxed}");
            let mut places = Vec::new();
            for (at, line) in laid.lines().iter().enumerate() {
                for item in items(&laid, line) {
                    assert_eq!(line_of(&laid, item.start), at, "{inline} {item:?}");
                    places.push(item.start);
                    places.push(item.end);
                }
            }
            places.sort();
            places.dedup();
            let mut scalars = Vec::new();
            let _ = walk(&laid, None, 0, |event| {
                if let Event::Scalar(c, s, _, _) = event {
                    scalars.push((c, s));
                }
                ControlFlow::Continue(())
            });
            for (i, &from) in places.iter().enumerate() {
                for &to in places.iter().skip(i + 1) {
                    let shown: String = scalars
                        .iter()
                        .filter(|(c, s)| *s >= from && *s < to && !c.is_whitespace())
                        .map(|(c, _)| *c)
                        .collect();
                    let mut out = copy_of(&laid, from, to);
                    out.retain(|c| !c.is_whitespace());
                    assert_eq!(out, shown, "{inline} {from:?} {to:?}");
                }
            }
        }
        // In a cell, beside the cell's own text and another cell.
        let laid = laid(&format!(
            "<table><tr><td>x <table {it}><tr><td>a<td>b</table> y<td>z</table>"
        ));
        assert_eq!(all(&laid), "x \na\tb y\tz");
        for (at, line) in laid.lines().iter().enumerate() {
            for item in items(&laid, line) {
                assert_eq!(line_of(&laid, item.start), at, "{item:?}");
            }
        }
        // In a float, its caption's text its line's: a copy from the
        // float's word into it parts them with the space laid, no newline.
        let laid = crate::page::tests::laid(&format!(
            "<div style=float:left>fghij <table {it}>\
             <caption style=caption-side:bottom>cde</caption></table></div>"
        ));
        let (j, _) = word_at(&laid, "j");
        let (_, c) = word_at(&laid, "c");
        assert_eq!(copy_of(&laid, j, c), "j c");
    }

    /// A table whose `float` is left keeps its place in flow, as layout
    /// lays it (td-layout/DESIGN.md §5, "Floats"): its caption's lines are
    /// not a float's, so a copy across them owes no float's newline: every
    /// copy is the table's without its `float`.
    #[test]
    fn a_floated_table_is_walked_in_flow() {
        for (markup, text) in [
            // In an inline-block, its caption the line's paragraph.
            (
                "<div style=float:left></div><span style=display:inline-block>\
                 <table{}><caption style=caption-side:bottom>cde </caption></table></span>k",
                "cdek",
            ),
            (
                "<p>one <span style=display:inline-block><table{}><caption>cap tion\
                 </caption><tr><td>a<td>b</table></span> two",
                "one cap tion\na\tb two",
            ),
            // At the top level, its caption and rows between paragraphs.
            (
                "<p>one</p><table{}><caption>cap tion</caption><tr><td>a<td>b</table><p>two",
                "one\ncap tion\na\tb\ntwo",
            ),
        ] {
            let floated = laid(&markup.replace("{}", " style=float:left"));
            let flow = crate::page::tests::laid(&markup.replace("{}", ""));
            assert_eq!(all(&floated), text, "{markup}");
            assert_eq!(all(&flow), text, "{markup}");
            let mut places = Vec::new();
            for line in floated.lines() {
                for item in items(&floated, line) {
                    places.push(item.start);
                    places.push(item.end);
                }
            }
            places.sort();
            places.dedup();
            for (i, &from) in places.iter().enumerate() {
                for &to in places.iter().skip(i + 1) {
                    assert_eq!(
                        copy_of(&floated, from, to),
                        copy_of(&flow, from, to),
                        "{markup} {from:?} {to:?}"
                    );
                }
            }
        }
    }

    /// A walk, a copy, a find and each move look at each line a bounded
    /// number of times, however many item-less lines a box holds.
    #[test]
    fn nested_lines_are_looked_at_a_bounded_number_of_times() {
        let brs = "<br>".repeat(20_000);
        for markup in [
            format!("<p>a <span style=display:inline-block>{brs}x</span> b</p>"),
            format!("<p>a <span style=float:left>{brs}x</span> b</p>"),
        ] {
            let laid = laid(&markup);
            let n = laid.lines().len();
            assert!(n > 20_000, "{n}");
            let walked = visits(|| {
                let _ = walk(&laid, None, 0, |_| ControlFlow::Continue(()));
            });
            assert!(walked <= 4 * n + 64, "walk: {walked} for {n} lines");
            let (first, last) = page_bounds(&laid);
            let copied = visits(|| {
                assert!(copy_of(&laid, first, last).contains("\nx"));
            });
            assert!(copied <= 4 * n + 64, "copy: {copied} for {n} lines");
            let finding = visits(|| assert_eq!(found_in(&laid, "x"), ["x"]));
            assert!(finding <= 4 * n + 64, "find: {finding} for {n} lines");
            let x = word_at(&laid, "x").0;
            for motion in [
                Move::Right,
                Move::Left,
                Move::WordRight,
                Move::WordLeft,
                Move::Down,
                Move::Up,
            ] {
                let moving = visits(|| {
                    mv(&laid, x, motion);
                });
                assert!(moving <= 8 * n + 64, "{motion:?}: {moving} for {n} lines");
            }
        }
    }

    /// The white space after a wrap goes before the line's first piece
    /// shown, not before nested lines met ahead of it: places run on.
    #[test]
    fn no_gap_goes_before_nested_lines_first_on_a_line() {
        let markup = "<p style=margin:0;width:120px>one two three \
             <span style=visibility:hidden>hidden </span>\
             <span style=float:left>FL</span>five six seven";
        let laid = laid(markup);
        let mut spans = Vec::new();
        let _ = walk(&laid, None, 0, |event| {
            if let Event::Scalar(_, s, e, _) | Event::Alt(_, s, e) | Event::Gap(_, s, e) = event {
                spans.push((s, e));
            }
            ControlFlow::Continue(())
        });
        assert!(spans.windows(2).all(|w| w[0].1 <= w[1].0), "{spans:?}");
        assert_eq!(all(&laid), "one two three\nFL\nfive six seven");
        assert_eq!(found_in(&laid, "five six"), ["five six"]);
    }

    /// A hidden inline-block is walked as one, its lines in its line.
    #[test]
    fn a_hidden_inline_block_is_walked_as_one() {
        let laid =
            laid("<p>ab <span style=display:inline-block;visibility:hidden>hid</span> cd</p>");
        let mut entered = Vec::new();
        let _ = walk(&laid, None, 0, |event| {
            if let Event::Enter(kind) = event {
                entered.push(kind);
            }
            ControlFlow::Continue(())
        });
        assert_eq!(entered, [Nested::Inline]);
        assert_eq!(all(&laid), "ab  cd");
    }

    /// A box's last line with no item (a `<br>` last in it) lies within
    /// the line holding the box, walked with the box's lines.
    #[test]
    fn a_box_s_last_line_with_no_item_is_walked_within_it() {
        let p = "<p style=margin:0;width:120px>one ";
        for (markup, text) in [
            (
                format!("{p}<span style=float:left>FL<br></span>two three four five six seven"),
                "one \nFL\ntwo three four five six seven",
            ),
            (
                format!("{p}two <span style=display:inline-block>IB<br></span> three four five six seven"),
                "one two IB three four five six seven",
            ),
        ] {
            let laid = laid(&markup);
            assert_eq!(all(&laid), text, "{markup}");
            assert_eq!(found_in(&laid, "three four"), ["three four"], "{markup}");
            let lines = laid.lines();
            let empty = lines
                .iter()
                .position(|line| items(&laid, line).next().is_none())
                .unwrap();
            assert_eq!(laid.outer(empty), Some(0), "{markup}");
        }
    }

    /// Each move, chained from every place, goes one way until it stays:
    /// out of and back past nested boxes, in tree order.
    #[test]
    fn moves_chained_from_every_place_are_monotonic() {
        let ib = "<span style=display:inline-block>IB</span>";
        let pages = [
            format!("<p>one {ib} two</p><p>next</p>"),
            "<p>c <span style=float:left>pq </span>lmno".to_string(),
            format!("<p>one</p><p>{ib} two</p>"),
            "<p style=margin:0;width:120px>one two three <span style=visibility:hidden>hidden \
             </span><span style=float:left>FL</span>five six seven"
                .to_string(),
            "<p style=margin:0;width:120px>one <span style=float:left>FL<br></span>two three \
             four five six seven"
                .to_string(),
            "<p>lead<p style=margin:0;width:197px><span style=float:left;width:40px><br>lmno ab \
             <br>k <a href=/l style=display:inline-block></a>ab <img width=12 height=8>k \
             <span style=display:inline-block></span></span>tail words end"
                .to_string(),
            "<p>lead<p style=margin:0;width:103px>k <img width=12 height=8>lmno ab \
             <span style=float:left;width:40px><br>k <br><img width=12 height=8>\
             <img width=12 height=8><a href=/l style=display:inline-block>lmno k fghij k \
             <img width=12 height=8><br>ab <br>cde fghij </a><img width=12 height=8>ab ab ab \
             fghij ab <span style=float:left;width:40px>k fghij <img width=12 height=8>k \
             </span></span>tail words end"
                .to_string(),
            "<p style=margin:0;width:120px>one <span style=display:inline-block>aa<br>bb</span> \
             two three four five six<p>next"
                .to_string(),
            "<p>lead<p style=margin:0;width:91px><span style=float:left;width:40px>k \
             <span style=float:right><span style=display:inline-block></span></span></span>\
             tail words end"
                .to_string(),
        ];
        for markup in &pages {
            let laid = laid(markup);
            let (first, last) = page_bounds(&laid);
            let mut places = vec![first, last];
            for line in laid.lines() {
                for item in items(&laid, line) {
                    places.push(item.start);
                    places.push(item.end);
                }
            }
            places.sort();
            places.dedup();
            for &from in &places {
                for (motion, ahead) in [
                    (Move::Right, true),
                    (Move::WordRight, true),
                    (Move::Down, true),
                    (Move::Left, false),
                    (Move::WordLeft, false),
                    (Move::Up, false),
                ] {
                    let mut at = from;
                    let mut steps = 0;
                    loop {
                        let to = mv(&laid, at, motion);
                        if to == at {
                            break;
                        }
                        assert_eq!(to > at, ahead, "{markup}: {motion:?} {at:?} to {to:?}");
                        at = to;
                        steps += 1;
                        assert!(steps <= 2 * places.len() + 64, "{markup}: {motion:?}");
                    }
                    let edge = if ahead { last } else { first };
                    if matches!(motion, Move::Right | Move::Left) {
                        assert_eq!(at, edge, "{markup}: {motion:?} from {from:?}");
                    }
                }
            }
        }
        let down = laid(&pages[0]);
        let two = word_at(&down, "two").1;
        assert_eq!(mv(&down, two, Move::Down), word_at(&down, "next").1);
        let left = laid(&pages[2]);
        let (_, one) = word_at(&left, "one");
        let box_start = items(&left, &left.lines()[1]).next().unwrap().start;
        assert_eq!(mv(&left, box_start, Move::Left), one);
        // Up from a paragraph's start: the latest paragraph start before
        // it, an inline-block's laid among its holder's lines.
        let up = laid(&pages[7]);
        let next = word_at(&up, "next").0;
        assert_eq!(mv(&up, next, Move::Up), word_at(&up, "bb").0);
    }

    /// A copy ending at a line's start, its first piece hidden or not,
    /// ends in the newline before it; one ending past it, not.
    #[test]
    fn a_copy_ending_at_a_line_s_start_ends_in_its_newline() {
        let laid = laid("<p>aaa</p><p><span style=visibility:hidden>hid </span>bbb</p>");
        let (first, _) = page_bounds(&laid);
        let start = items(&laid, &laid.lines()[1]).next().unwrap().start;
        assert_eq!(copy_of(&laid, first, start), "aaa\n");
        assert_eq!(copy_of(&laid, first, word_at(&laid, "bbb").0), "aaa");
    }

    /// An inline-block last in another, or in a float, followed at once
    /// by text, an image or an inline-block link on the outer line: each
    /// piece after it starts on its own line, so a word, a paragraph and
    /// a step right there are that line's.
    #[test]
    fn a_nested_last_inline_block_ends_before_what_follows() {
        let ib = "style=display:inline-block";
        let body = "<body style=margin:0><p style=margin:0>";
        for markup in [
            format!("{body}x <span {ib}><span {ib}>y</span></span>zebra crossing"),
            format!("{body}x <span {ib}><span {ib}>y</span></span><img width=10 height=10>z"),
            format!("{body}<a href=/a {ib}><span {ib}>icon</span></a><a href=/b {ib}>Next</a> end"),
            format!(
                "<body style=margin:0><ul style=margin:0><li {ib}><a href=/h {ib}>Home</a></li>\
                 <li {ib}><a href=/x>About</a></li></ul>"
            ),
            format!("{body}foo <span style=float:left>a <span {ib}>b</span></span>bar baz"),
        ] {
            let laid = laid(&markup);
            for (at, line) in laid.lines().iter().enumerate() {
                for item in items(&laid, line) {
                    assert_eq!(line_of(&laid, item.start), at, "{markup} {item:?}");
                }
            }
        }
        let laid = laid(&format!(
            "{body}x <span {ib}><span {ib}>y</span></span>zebra crossing"
        ));
        let zebra = laid.lines()[0]
            .fragments
            .iter()
            .find(|f| fragment_text(&laid, f).starts_with("zebra"))
            .unwrap();
        let at = Place::new(&laid, zebra.node, zebra.range.start);
        assert_eq!(line_of(&laid, at), 0);
        let mut m = measure(WANT);
        let (s, e) = word(&laid, &mut m, at).unwrap();
        assert_eq!((s, e.offset), (at, 5));
        let (_, end) = paragraph_places(&laid, at).unwrap();
        assert_eq!(end.offset, 14);
        assert_eq!(mv(&laid, at, Move::Right).offset, 1);
    }

    /// Every laid piece of generated pages of floats, inline-blocks
    /// nested in each other, last in their holders and followed at once
    /// by text, images and links, is found on its own line.
    #[test]
    fn generated_nested_boxes_find_every_piece_on_its_line() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % n
        };
        let mut wrong = Vec::new();
        for page in 0..200 {
            let mut markup = String::from("<body style=margin:0><p style=margin:0>");
            let mut open = Vec::new();
            for _ in 0..12 + next(20) {
                match next(9) {
                    0 | 1 if open.len() < 3 => {
                        markup.push_str("<span style=display:inline-block>");
                        open.push("</span>");
                    }
                    2 if open.len() < 3 => {
                        markup.push_str("<span style=float:left;width:30px>");
                        open.push("</span>");
                    }
                    3 if open.len() < 3 => {
                        markup.push_str("<a href=/l style=display:inline-block>");
                        open.push("</a>");
                    }
                    4 | 5 => {
                        if let Some(close) = open.pop() {
                            markup.push_str(close);
                        }
                    }
                    6 => markup.push_str("<img width=12 height=8>"),
                    7 => markup.push(' '),
                    _ => markup.push_str(["ab", "cd ef", "ghij", "k"][next(4) as usize]),
                }
            }
            while let Some(close) = open.pop() {
                markup.push_str(close);
            }
            markup.push_str("tail word");
            let laid = laid(&markup);
            for (at, line) in laid.lines().iter().enumerate() {
                for item in items(&laid, line) {
                    if line_of(&laid, item.start) != at {
                        wrong.push((page, at, markup.clone()));
                    }
                }
            }
        }
        assert!(
            wrong.is_empty(),
            "{} wrong: {:?}",
            wrong.len(),
            wrong.first()
        );
    }

    /// A point in an inline-block falls on its own line, not on the line
    /// holding it, whose box holds it too.
    #[test]
    fn a_point_in_an_inline_block_falls_on_its_lines() {
        let laid = laid(
            "<body style=margin:0><p style=margin:0>one <span \
             style=display:inline-block;padding:4px>aa bb<br>cc</span> two",
        );
        let mut m = measure(WANT);
        let lines = laid.lines();
        for at in [1, 2] {
            let line = &lines[at];
            let f = &line.fragments[0];
            let (x, y) = (f.x + 1.0, line.y + line.height / 2.0);
            assert_eq!(line_at(&laid, x, y), Some(at));
            let (place, item) = place_at(&laid, &mut m, x, y).unwrap();
            assert_eq!(place.node, Some(f.node));
            assert!(matches!(item.map(|i| i.piece), Some(Piece::Text(0))));
            assert_eq!(line_of(&laid, place), at);
            assert_eq!(locate(&laid, &mut m, place).unwrap().0, at);
        }
        // In its padding, off its lines: the line holding it, on its
        // placeholder.
        let r = &lines[0].replaced[0];
        assert!(r.atomic);
        let (place, item) = place_at(&laid, &mut m, r.x + 1.0, r.y + 1.0).unwrap();
        assert!(matches!(item.map(|i| i.piece), Some(Piece::Replaced(0))));
        assert_eq!(line_of(&laid, place), 0);
    }

    /// A selection ending at the start of a paragraph's last line, wrapped
    /// or split, copies no newline after it.
    #[test]
    fn a_selection_ending_at_a_last_lines_start_copies_no_newline() {
        for markup in [
            "<body style=margin:0><p style=margin:0;width:80px>one two three four five six\
             <p style=margin:0>next",
            "<body style=margin:0><p style=margin:0;width:120px>\
             http://example.com/a/very/long/path/name<p style=margin:0>next",
        ] {
            let laid = laid(markup);
            let lines = laid.lines();
            let block = lines[0].block();
            let last = lines.iter().rposition(|l| l.block() == block).unwrap();
            assert!(last > 0, "{markup}");
            let (start, _) = page_bounds(&laid);
            let (end, _) = bounds(&laid, &lines[last]).unwrap();
            let mut out = String::new();
            let _ = copy(&laid, start, end, &mut |text| {
                out.push_str(text);
                ControlFlow::Continue(())
            });
            assert!(!out.is_empty() && !out.ends_with('\n'), "{markup} {out:?}");
        }
    }

    /// An empty item's marker is a paragraph of its own, copied when the
    /// selection spans its item (td-web/DESIGN.md §10, "Copying").
    #[test]
    fn an_empty_items_marker_is_a_paragraph_of_its_own() {
        let laid = laid("<ol><li>first<li><li>third</ol><p>after");
        assert_eq!(all(&laid), "1. first\n2.\n3. third\nafter");
        // From the third item on, the empty one is not held.
        let (start, end) = page_bounds(&laid);
        let third = paragraph_places_of_line(&laid, 1).unwrap().0;
        let mut out = String::new();
        let _ = copy(&laid, third, end, &mut |piece| {
            out.push_str(piece);
            ControlFlow::Continue(())
        });
        assert_eq!(out, "3. third\nafter");
        // Ending at the third item's start, it is.
        let mut out = String::new();
        let _ = copy(&laid, start, third, &mut |piece| {
            out.push_str(piece);
            ControlFlow::Continue(())
        });
        assert_eq!(out, "1. first\n2.\n");
    }

    /// `extents_until` stops where it is told, the rest unreplayed.
    #[test]
    fn extents_stop_where_they_are_told() {
        let laid = laid("<pre>abcdef</pre>");
        let line = &laid.lines()[0];
        let f = &line.fragments[0];
        let mut measure = crate::pipeline::tests::measure(crate::page::tests::WANT);
        let mut all = Vec::new();
        assert!(extents(&laid, &mut measure, line, f, |e| all.push(e.shown)));
        let mut told = Vec::new();
        assert!(extents_until(&laid, &mut measure, line, f, |e| {
            told.push(e.shown);
            if told.len() == 3 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        }));
        assert_eq!(told, all[..3]);
    }

    #[test]
    fn moves_step_scalars_words_and_paragraphs() {
        let laid = laid("<p>one two</p><p>three</p>");
        let (start, end) = page_bounds(&laid);
        let right = mv(&laid, start, Move::Right);
        assert_eq!(right.offset, 1);
        assert_eq!(mv(&laid, right, Move::Left), start);
        let word = mv(&laid, start, Move::WordRight);
        assert_eq!(word.offset, 3, "the end of the word after");
        assert_eq!(mv(&laid, word, Move::WordLeft), start);
        // Down: this paragraph's end, then the next one's.
        let down = mv(&laid, start, Move::Down);
        assert_eq!(paragraph_places(&laid, start).unwrap().1, down);
        let next = mv(&laid, down, Move::Down);
        assert_eq!(next, end);
        assert_eq!(
            mv(&laid, next, Move::Up),
            paragraph_places(&laid, end).unwrap().0
        );
        assert_eq!(mv(&laid, down, Move::Up), start);
        assert_eq!(word_of(&laid, right), "one");
        assert!(end > start);
    }

    fn word_of(laid: &Laid, at: Place) -> String {
        let (s, e) = word(laid, &mut measure(WANT), at).unwrap();
        let mut out = String::new();
        let _ = copy(laid, s, e, &mut |t| {
            out.push_str(t);
            ControlFlow::Continue(())
        });
        out
    }

    #[test]
    fn a_point_falls_between_scalars_by_halves_of_their_advances() {
        let laid = laid("<body style=margin:0><p style=margin:0>abc");
        let mut m = measure(WANT);
        // Cells are 8 px: x 11 is past b's half.
        let (place, under) = place_at(&laid, &mut m, 11.0, 4.0).unwrap();
        assert_eq!(place.offset, 1);
        assert!(under.is_some());
        let (place, _) = place_at(&laid, &mut m, 13.0, 4.0).unwrap();
        assert_eq!(place.offset, 2);
        let (place, under) = place_at(&laid, &mut m, 300.0, 4.0).unwrap();
        assert_eq!(place.offset, 3);
        assert!(under.is_none());
        assert_eq!(locate(&laid, &mut m, place).unwrap(), (0, 24.0));
        let (end, _) = place_at(&laid, &mut m, 0.0, 900.0).unwrap();
        assert_eq!(end.offset, 3, "below the page, the last line's end");
    }

    #[test]
    fn copy_measures_before_it_gathers() {
        let laid = laid("<p>abcdef");
        let (start, end) = page_bounds(&laid);
        let memory = Budget::new(1 << 20).unwrap();
        assert!(copied(&laid, start, end, 3, &memory).unwrap().is_none());
        assert_eq!(
            copied(&laid, start, end, 6, &memory)
                .unwrap()
                .unwrap()
                .as_str(),
            "abcdef"
        );
    }

    /// A page whose last line lays nothing, as a text page's final
    /// newline or a `br` ending a paragraph leaves: every place is on the
    /// line before it, and each use of a place finds that line.
    #[test]
    fn a_page_ending_in_a_line_with_nothing_on_it() {
        let page = Budget::new(64 << 20).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let made = text_page("203.0.113.7\n", WANT, &mut measure(WANT), &cancel, &page);
        let text = made.unwrap().laid.unwrap();
        let text: &Laid = &text;
        let words = laid("<body style=margin:0><p style=margin:0>only words<br></p>");
        for (laid, shown, first) in [(text, "203.0.113.7", "203"), (&words, "only words", "only")] {
            let lines = laid.lines();
            assert!(lines.len() >= 2, "{shown}: {}", lines.len());
            assert!(
                items(laid, lines.last().unwrap()).next().is_none(),
                "{shown}"
            );
            let (start, end) = page_bounds(laid);
            assert_eq!(line_of(laid, start), 0, "{shown}");
            assert_eq!(line_of(laid, end), 0, "{shown}");
            let one = Place::new(laid, start.node.unwrap(), 1);
            assert_eq!(line_of(laid, one), 0, "{shown}");
            // Copy: Ctrl+A and a part.
            assert_eq!(all(laid), shown);
            assert_eq!(copy_of(laid, one, end), shown.get(1..).unwrap());
            // Words and paragraphs.
            assert_eq!(word_of(laid, one), first);
            assert_eq!(paragraph_places(laid, one), Some((start, end)), "{shown}");
            assert_eq!(paragraph_places_of_line(laid, 0), Some((start, end)));
            // Moves.
            assert_eq!(mv(laid, start, Move::Right), one, "{shown}");
            assert_eq!(mv(laid, one, Move::Left), start, "{shown}");
            assert_eq!(
                mv(laid, start, Move::WordRight).offset as usize,
                first.len()
            );
            assert_eq!(mv(laid, start, Move::Down), end, "{shown}");
            assert_eq!(mv(laid, end, Move::Up), start, "{shown}");
            assert_eq!(mv(laid, end, Move::Right), end, "the page's end stays");
            let before = mv(laid, end, Move::Left);
            assert_eq!(before.offset as usize, shown.len() - 1, "{shown}");
            // Where places are drawn, and the places at points.
            let mut m = measure(WANT);
            let line = &lines[0];
            let left = horizontal(line).0;
            let width = 8.0 * shown.len() as f64;
            assert_eq!(locate(laid, &mut m, one), Some((0, left + 8.0)));
            assert_eq!(locate(laid, &mut m, end), Some((0, left + width)));
            let y = line.y + 1.0;
            assert_eq!(line_at(laid, left + 10.0, y), Some(0));
            let (place, under) = place_at(laid, &mut m, left + 10.0, y).unwrap();
            assert_eq!(place, one, "{shown}");
            let under = under.unwrap();
            assert_eq!(line_of(laid, under.start), 0, "the piece under it");
            let (below, _) = place_at(laid, &mut m, left + 10.0, 900.0).unwrap();
            assert_eq!(below, end, "{shown}");
        }
    }

    /// An image on such a page is found on its line, as a click on it
    /// opens it.
    #[test]
    fn an_image_on_a_page_ending_in_an_empty_line_is_on_its_line() {
        let laid = laid("<body style=margin:0><p style=margin:0>a<img alt=picture><br></p>");
        let mut m = measure(WANT);
        let line = &laid.lines()[0];
        let r = &line.replaced[0];
        let (_, under) = place_at(&laid, &mut m, r.x + 1.0, line.y + 1.0).unwrap();
        let under = under.unwrap();
        assert_eq!(under.piece, Piece::Replaced(0));
        assert_eq!(line_of(&laid, under.start), 0);
        assert_eq!(all(&laid), "apicture");
    }

    /// Hidden text is laid but not painted, so not copied; nor is the
    /// marker of an item hidden so.
    #[test]
    fn hidden_text_is_not_copied() {
        let laid = laid(
            "<p>seen needle<p style=visibility:hidden>hidden needle\
             <p style=display:none>gone needle",
        );
        assert_eq!(all(&laid), "seen needle");
        let laid = laid_for(
            "<p>a <span style=visibility:hidden>b</span>c<p>d",
            Source::Markup,
            WANT,
        );
        assert!(!all(&laid).contains('b'));
        let list = laid_for(
            "<ol><li>first<li style=visibility:hidden>second</ol><p>after",
            Source::Markup,
            WANT,
        );
        assert_eq!(all(&list), "1. first\nafter");
    }

    /// A mark laid with no advance is never parted from the scalar
    /// before it: no move, word or point puts a place between them. The
    /// tests' cell lays every scalar 8 px, so the mark here is laid with
    /// no advance by letter-spacing.
    #[test]
    fn no_place_falls_between_a_base_and_its_mark_of_no_advance() {
        let laid = laid(
        "<body style=margin:0><p style=margin:0>e<span style=letter-spacing:-8px>\u{301}</span>x y",
    );
        let line = &laid.lines()[0];
        let widths: Vec<f64> = line.fragments.iter().map(|f| f.width).collect();
        assert_eq!(widths.get(..2), Some(&[8.0, 0.0][..]), "{widths:?}");
        let (start, _) = page_bounds(&laid);
        let mark = line.fragments[1].node;
        let joined = Place::new(&laid, mark, 2);
        // Right steps over the base and its mark, Left back over both.
        let right = mv(&laid, start, Move::Right);
        assert_eq!(right, joined);
        assert_eq!(copy_of(&laid, start, right), "e\u{301}");
        let x = mv(&laid, right, Move::Right);
        assert_eq!(copy_of(&laid, start, x), "e\u{301}x");
        let back = mv(&laid, x, Move::Left);
        assert_eq!(copy_of(&laid, start, back), "e\u{301}");
        assert_eq!(mv(&laid, back, Move::Left), start);
        // The mark is no letter, but belongs to its base's word.
        assert_eq!(word_of(&laid, start), "e\u{301}x");
        assert_eq!(
            copy_of(&laid, start, mv(&laid, start, Move::WordRight)),
            "e\u{301}x"
        );
        // Past the base's middle, the place after its mark; before it,
        // the base's start.
        let mut m = measure(WANT);
        assert_eq!(place_at(&laid, &mut m, 6.0, 4.0).unwrap().0, joined);
        assert_eq!(place_at(&laid, &mut m, 2.0, 4.0).unwrap().0, start);
        // A place found between them, as find's can be, moves out.
        let between = Place::new(&laid, line.fragments[0].node, 1);
        assert_eq!(mv(&laid, between, Move::Right), joined);
        assert_eq!(mv(&laid, between, Move::Left), start);
    }

    /// A line beginning with a mark of no advance still has a start a
    /// point reaches.
    #[test]
    fn a_line_beginning_with_a_mark_of_no_advance_has_a_start() {
        let laid = laid(
        "<body style=margin:0><p style=margin:0><span style=letter-spacing:-8px>\u{301}</span>ab",
    );
        let (start, _) = page_bounds(&laid);
        let mut m = measure(WANT);
        assert_eq!(place_at(&laid, &mut m, 0.0, 4.0).unwrap().0, start);
        let a = laid.lines()[0].fragments[1].node;
        assert_eq!(
            place_at(&laid, &mut m, 2.0, 4.0).unwrap().0,
            Place::new(&laid, a, 0)
        );
        assert_eq!(
            place_at(&laid, &mut m, 6.0, 4.0).unwrap().0,
            Place::new(&laid, a, 1)
        );
        // With no base before it, the mark is a step of its own.
        assert_eq!(
            mv(&laid, start, Move::Right),
            Place::new(&laid, laid.lines()[0].fragments[0].node, 2)
        );
    }

    /// Cells of two CSS tables, each in a row CSS left anonymous, are
    /// two rows; a CSS table's cells in one anonymous row are one.
    #[test]
    fn css_tables_anonymous_rows_join_only_their_own_cells() {
        let cell = "<div style=display:table-cell>";
        let two = laid(&format!(
        "<div style=display:table>{cell}a</div></div><div style=display:table>{cell}b</div></div>"
    ));
        assert_eq!(all(&two), "a\nb");
        let one = laid(&format!(
            "<div style=display:table>{cell}a</div>{cell}b</div></div>"
        ));
        assert_eq!(all(&one), "a\tb");
    }

    /// A cell laying only an empty line owes its tab, which joins the
    /// next cell's; empty cells give theirs between laid cells and none
    /// before a row's first or after its last.
    #[test]
    fn empty_cells_give_their_tabs_between_laid_ones() {
        assert_eq!(
            all(&laid("<table><tr><td>a<td><br><td>c</table>")),
            "a\t\tc"
        );
        assert_eq!(
            all(&laid(
                "<table><tr><td><td>a<td><td><td>b<td></table><p>after"
            )),
            "a\t\t\tb\nafter"
        );
        assert_eq!(
            all(&laid("<table><tr><td>a<td></tr><tr><td><td>b</table>")),
            "a\nb"
        );
    }

    /// `line_at` by its index of spans finds what a scan of every line
    /// finds: the nearest by height, then by x, the first of equals.
    #[test]
    fn the_line_at_a_point_is_the_nearest() {
        fn scanned(laid: &Laid, x: f64, y: f64) -> Option<usize> {
            let mut best: Option<(f64, f64, usize)> = None;
            for (at, line) in laid.lines().iter().enumerate() {
                if items(laid, line).next().is_none() {
                    continue;
                }
                let dy = if y < line.y {
                    line.y - y
                } else if y >= line.y + line.height {
                    y - (line.y + line.height) + f64::MIN_POSITIVE
                } else {
                    0.0
                };
                let (left, right) = horizontal(line);
                let dx = (left - x).max(x - right).max(0.0);
                if best.is_none_or(|(by, bx, _)| dy < by || (dy == by && dx < bx)) {
                    best = Some((dy, dx, at));
                }
            }
            best.map(|(_, _, at)| at)
        }
        let mut markup = String::from("<body style=margin:0>");
        for n in 0..150 {
            markup.push_str(&format!("<p style=margin:{}px>line {n}<br><br>", n % 7));
            if n % 40 == 0 {
                markup.push_str("<table><tr><td>a<br>b<br>c<td>d<td>e<br>f</table>");
            }
        }
        let laid = laid(&markup);
        assert!(laid.lines().len() > 3 * CHUNK);
        let height = laid.layout.height;
        let mut y = -20.0;
        while y < height + 20.0 {
            for x in [-5.0, 3.0, 40.0, 90.0, 500.0] {
                assert_eq!(line_at(&laid, x, y), scanned(&laid, x, y), "{x} {y}");
            }
            y += 3.25;
        }
    }

    /// A float's edges set it off from the text around it, never a cell's
    /// tabs: a float holding a table keeps its rows' cells a tab apart and
    /// their paragraphs a space apart, and a float in a cell is set off
    /// from the next cell by tabs.
    #[test]
    fn a_float_s_edges_keep_a_row_s_cells_a_tab_apart() {
        for (markup, text) in [
            (
                "<div style=float:left;width:300px><table><tr><td>a</td><td>b</td></tr>\
                 <tr><td>c</td><td>d</td></tr></table></div><p>after</p>",
                "a\tb\nc\td\nafter",
            ),
            (
                "<div style=float:left;width:300px><table><tr><td>a<br>x</td><td>b</td></tr>\
                 </table></div>",
                "a x\tb",
            ),
            (
                "<div style=float:left><table><tr><td>a</td><td></td><td>b</td></tr></table></div>",
                "a\t\tb",
            ),
            (
                "<table><tr><td><div style=float:left>a</div></td><td>b</td></tr></table>",
                "a\tb",
            ),
            (
                "<table><td>pq </td><td><span style=float:left><br></span>ab </td></table>",
                "pq\tab",
            ),
            (
                "<div style=display:inline-block><div style=float:left><table><tr><td>a</td>\
                 <td>b</td></tr></table></div></div>",
                "a\tb",
            ),
            (
                "<table><tr><td>x <span style=float:left>fl</span> y</td><td>z</td></tr></table>",
                "x fl y\tz",
            ),
            // Back from an inline-block's table, in the cell holding it.
            (
                "<table><tr><td>s0 <span style=display:inline-block><table><tr><td>x</td></tr>\
                 </table></span></td><td>z</td></tr></table>",
                "s0 \nx\tz",
            ),
        ] {
            assert_eq!(all(&laid(markup)), text, "{markup}");
        }
        // A float's run of paragraphs, its table's cells, is entered once.
        let laid = laid(
            "<div style=float:left;width:300px><table><tr><td>a</td><td>b</td></tr>\
             <tr><td>c</td><td>d</td></tr></table></div><p>after</p>",
        );
        let mut entered = 0;
        let _ = walk(&laid, None, 0, |event| {
            if let Event::Enter(Nested::Float) = event {
                entered += 1;
            }
            ControlFlow::Continue(())
        });
        assert_eq!(entered, 1);
    }

    /// The white space of a wrap before a float's lines goes where it
    /// lies, before them, so a float with nothing shown leaves it be; one
    /// with text puts its newline in its place.
    #[test]
    fn a_wrap_s_white_space_goes_before_a_float_s_lines() {
        for width in [40, 88] {
            let p = format!("<p style=margin:0;width:{width}px>aaa bbb ccc ");
            let markup = format!("{p}<span style=float:left><br></span>ddd eee");
            assert_eq!(all(&laid(&markup)), "aaa bbb ccc ddd eee", "{markup}");
            let markup = format!("{p}<span style=float:left>fl</span>ddd eee");
            assert_eq!(all(&laid(&markup)), "aaa bbb ccc\nfl\nddd eee", "{markup}");
        }
        for hidden in [
            "three <span style=visibility:hidden>hidden </span>",
            "three<span style=visibility:hidden> hidden</span> ",
        ] {
            let markup = format!(
                "<p style=margin:0;width:120px>one two {hidden}<span style=float:left><br></span>\
                 five six seven"
            );
            assert_eq!(
                all(&laid(&markup)),
                "one two three five six seven",
                "{markup}"
            );
        }
        let markup = "<div style=width:177px><table><tr><td></td><td>s26 lmno <table><tr><td>s27 \
             fghij fghij cde  e27</td></tr></table>cde lmno <div style=float:left;width:60px>\
             <br><br></div> e26</td><td>s28 lmno cde fghij  e28</td></tr></table>";
        assert_eq!(
            all(&laid(markup)),
            "s26 lmno\ns27 fghij fghij cde e27\ncde lmno e26\ts28 lmno cde fghij e28"
        );
    }

    /// A move back walks from near the place: its own paragraph, or the
    /// one before it, not the whole of the paragraph holding it.
    #[test]
    fn a_move_back_walks_from_near_the_place() {
        let markup = format!(
            "<p>{}",
            "<span style=display:inline-block>w</span> ".repeat(2_000)
        );
        let laid = laid(&markup);
        let mut last = None;
        for line in laid.lines() {
            for item in items(&laid, line) {
                if matches!(item.piece, Piece::Text(_)) {
                    last = Some(item.start);
                }
            }
        }
        let at = last.unwrap();
        for motion in [Move::Left, Move::WordLeft, Move::Up] {
            let walked = pieces(|| {
                assert!(mv(&laid, at, motion) < at);
            });
            assert!(walked <= 16, "{motion:?}: {walked} pieces");
        }
    }

    /// A float with nothing shown parts nothing, and one with nothing
    /// shown in another keeps no paragraph end of that one from its next.
    #[test]
    fn a_float_with_nothing_shown_parts_nothing() {
        for (markup, text) in [
            (
                "<p>ab<span style=float:left;visibility:hidden>zz</span>cd",
                "abcd",
            ),
            ("<p>x <span style=float:left><br></span> y", "x y"),
            (
                "<p>q <span style=float:left>a <span style=float:left;visibility:hidden>h\
                 </span></span>b",
                "q \na\nb",
            ),
            (
                "<p>q <span style=float:left>cde<br><span style=float:right><br></span>k</span>b",
                "q \ncde\nk\nb",
            ),
            (
                "<p>q <span style=float:left>a <span style=float:left>x</span>y</span>b",
                "q \na \nx\ny\nb",
            ),
            // An empty inline-block is a piece, as a picture is.
            (
                "<p>x <span style=float:left><span style=display:inline-block></span></span>y",
                "x \ny",
            ),
        ] {
            assert_eq!(all(&laid(markup)), text, "{markup}");
        }
    }

    /// The line a place lies in looks past a run of lines with no item
    /// once, however many floats' lines it climbs out of.
    #[test]
    fn a_place_s_line_looks_past_item_less_lines_once() {
        let markup = format!(
            "<p>a {}{}<span style=white-space:pre>{}</span>c",
            "<span style=float:left>f ".repeat(40),
            "</span>g ".repeat(40),
            "\n".repeat(5_000)
        );
        let laid = laid(&markup);
        let lines = laid.lines().len();
        assert!(lines > 5_000);
        let mut worst = 0;
        for line in laid.lines() {
            for item in items(&laid, line) {
                for place in [item.start, item.end] {
                    worst = worst.max(visits(|| {
                        line_of(&laid, place);
                    }));
                }
            }
        }
        assert!(worst <= lines, "{worst} visits for {lines} lines");
    }

    /// A paragraph's run of floats' lines is entered once, its block's
    /// float or inline-block found climbing to an ancestor once climbed,
    /// for blocks sharing a parent and for each in a box of its own.
    #[test]
    fn sibling_paragraphs_find_their_float_once() {
        for paragraph in ["<p>x</p>", "<div><p>x</p></div>"] {
            let markup = format!(
                "{}{}<p><span style=float:left>f</span>y</p>{}",
                "<div>".repeat(60),
                paragraph.repeat(2_000),
                "</div>".repeat(60)
            );
            let laid = laid(&markup);
            let lines = laid.lines().len();
            let walked = visits(|| {
                let _ = walk(&laid, None, 0, |_| ControlFlow::Continue(()));
            });
            assert!(
                walked <= 4 * lines,
                "{paragraph}: {walked} visits for {lines} lines"
            );
        }
    }

    /// A list marker is put as text is: after the edge or the paragraph's
    /// end a float owes before it, and right before its item's text.
    #[test]
    fn a_marker_follows_what_a_float_owes_before_it() {
        for (markup, text) in [
            (
                "<div style=float:left;width:100px>FL</div><ul><li>one</li><li>two</li></ul>",
                "FL\n• one\n• two",
            ),
            (
                "<p style=margin:0>intro</p><div style=float:right;width:150px><ul><li>one</li>\
                 <li>two</li></ul></div><p style=margin:0>after</p>",
                "intro\n• one\n• two\nafter",
            ),
            (
                "<ul><li><span style=float:left>FL</span>one</li><li>two</li></ul>",
                "FL\n• one\n• two",
            ),
            (
                "<ul><li>one <span style=float:right>FL</span></li><li>two</li></ul>",
                "• one\nFL\n• two",
            ),
            (
                "<span style=float:left>FL</span><ol><li>one</li><li>two</li></ol>",
                "FL\n1. one\n2. two",
            ),
            (
                "<div style=float:left;width:100px><ul><li>a</li><li>b</li></ul></div>\
                 <ul><li>one</li></ul>",
                "• a\n• b\n• one",
            ),
        ] {
            assert_eq!(all(&laid(markup)), text, "{markup}");
        }
    }

    /// A copy ending where a line starts ends in a newline exactly when
    /// one scalar more would go on after one: after a float's text, but
    /// not mid-paragraph past an inline-block or an edge owed before it.
    #[test]
    fn a_copy_ending_at_a_line_s_start_holds_the_newline_it_would_put() {
        for (markup, from, to, text) in [
            ("<span style=float:left>k </span>pq", "k", "pq", "k\n"),
            (
                "<div style=float:left;width:100px>FL</div><p style=margin:0>after</p>",
                "FL",
                "after",
                "FL\n",
            ),
            (
                "<p style=margin:0;width:120px>one <span style=float:left>FL</span>two three \
                 four five six",
                "one",
                "FL",
                "one \n",
            ),
            (
                "<p style=margin:0;width:120px>one <span style=display:inline-block>IB<br>\
                 </span> two three four five six",
                "one",
                "three",
                "one IB two ",
            ),
        ] {
            let laid = laid(markup);
            let (start, end) = (word_at(&laid, from).0, word_at(&laid, to).0);
            assert_eq!(copy_of(&laid, start, end), text, "{markup}");
        }
        // Ending at a wrapped line led by hidden text, where no scalar
        // shown starts it, owes no paragraph's end nor an earlier edge.
        let hidden = "<span style=visibility:hidden>";
        for (markup, word, text) in [
            (
                "<body style=margin:0;width:173px><img width=12 height=8 alt=IMG>\
                 <span style=float:right><span style=display:inline-block></span></span>\
                 k fghij fghij lmno ab"
                    .to_string(),
                "ab",
                "IMG\nk fghij fghij lmno ",
            ),
            (
                format!(
                    "<body style=margin:0;width:173px><img width=12 height=8 alt=IMG>\
                     <span style=float:right><span style=display:inline-block></span></span>\
                     k fghij fghij lmno {hidden}ab</span> cd"
                ),
                "ab",
                "IMG\nk fghij fghij lmno",
            ),
            (
                format!(
                    "<p style=margin:0;width:120px>one <span style=display:inline-block>IB<br>\
                     </span> two {hidden}three four</span> five six"
                ),
                "three",
                "one IB two",
            ),
        ] {
            let laid = laid(&markup);
            let at = word_at(&laid, word).0;
            assert!(
                laid.lines()
                    .iter()
                    .any(|line| bounds(&laid, line).map(|b| b.0) == Some(at)),
                "{markup}"
            );
            assert_eq!(copy_of(&laid, page_bounds(&laid).0, at), text, "{markup}");
        }
    }

    /// A copy ending at the start of a wrapped line of hidden text, with
    /// nothing shown after it in its paragraph, holds no newline: a line
    /// starting at the end owes nothing for its own paragraph's end.
    #[test]
    fn a_copy_ending_at_a_hidden_line_s_start_owes_its_paragraph_nothing() {
        let laid = laid(
            "<p style=margin:0;width:96px>cde fgh <span style=visibility:hidden>hid den </span>\
             </p><p style=margin:0>lm</p>",
        );
        let den = word_at(&laid, "den").0;
        assert_eq!(line_of(&laid, den), 1, "den starts the second line");
        assert_eq!(copy_of(&laid, word_at(&laid, "cde").0, den), "cde fgh ");
    }

    /// A copy whose consumer breaks is handed nothing more, not even the
    /// newline a copy ending where a line starts would close with.
    #[test]
    fn a_copy_stops_where_its_consumer_breaks() {
        let laid = laid("<p>a <span style=display:inline-block>b<br><br></span>c<p>d");
        let (first, _) = page_bounds(&laid);
        let d = word_at(&laid, "d").0;
        assert_eq!(copy_of(&laid, first, d), "a bc\n");
        let mut handed = Vec::new();
        let flow = copy(&laid, first, d, &mut |text| {
            handed.push(text.to_string());
            if text == "c" {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        assert!(flow.is_break());
        assert_eq!(handed.last().map(String::as_str), Some("c"));
    }

    /// Content for `generated_tables_keep_their_cells_apart`: words,
    /// breaks, pictures, floats, inline-blocks and tables, nested up to
    /// three deep; each table's rows, by their table (where it starts in
    /// the markup) and their cells' ids, go to `rows`.
    #[allow(clippy::type_complexity)]
    fn table_content(
        next: &mut dyn FnMut(u64) -> u64,
        depth: usize,
        markup: &mut String,
        rows: &mut Vec<(usize, Vec<Option<usize>>)>,
        ids: &mut usize,
    ) {
        const WORDS: &[&str] = &["ab", "cde", "fghij", "k", "lmno"];
        for _ in 0..2 + next(6) {
            match next(12) {
                0 if depth < 3 => {
                    markup.push_str(
                        [
                            "<div style=float:left;width:60px>",
                            "<div style=float:right>",
                        ][next(2) as usize],
                    );
                    table_content(next, depth + 1, markup, rows, ids);
                    markup.push_str("</div>");
                }
                1 if depth < 3 => {
                    markup.push_str("<div style=display:inline-block>");
                    table_content(next, depth + 1, markup, rows, ids);
                    markup.push_str("</div>");
                }
                2 | 3 if depth < 3 => {
                    let table = markup.len();
                    markup.push_str("<table>");
                    for _ in 0..1 + next(2) {
                        markup.push_str("<tr>");
                        let mut row = Vec::new();
                        for _ in 0..1 + next(3) {
                            if next(5) == 0 {
                                markup.push_str("<td></td>");
                                row.push(None);
                                continue;
                            }
                            let id = *ids;
                            *ids += 1;
                            markup.push_str(&format!("<td>s{id} "));
                            table_content(next, depth + 1, markup, rows, ids);
                            markup.push_str(&format!(" e{id}</td>"));
                            row.push(Some(id));
                        }
                        markup.push_str("</tr>");
                        rows.push((table, row));
                    }
                    markup.push_str("</table>");
                }
                4 => markup.push_str("<br>"),
                5 => markup.push_str("<img width=12 height=8>"),
                _ => {
                    markup.push_str(WORDS[next(5) as usize]);
                    markup.push(' ');
                }
            }
        }
    }

    /// Whether `text` holds `needle` as whole tokens: no letter or digit
    /// either side.
    fn holds_token(text: &str, needle: &str) -> bool {
        text.match_indices(needle).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + needle.len()..].chars().next();
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
    }

    /// Generated tables in floats and inline-blocks, and floats,
    /// inline-blocks and tables in their cells, among wrapped text: two
    /// cells of one row copy a tab apart, one more for each empty cell
    /// between, with no newline, and a row's last cell and the next row's
    /// first a newline apart, whatever lies in them; a walk is in tree
    /// order; a copy between any two places takes the scalars painted
    /// selected; and each move goes its way.
    #[test]
    fn generated_tables_keep_their_cells_apart() {
        let mut seed = 0x5851_f42d_4c95_7f2du64;
        let mut next = move |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % n
        };
        let mut m = measure(WANT);
        let mut checked = 0usize;
        for page in 0..120 {
            let width = 120 + next(240);
            let mut markup = format!("<body style=margin:0><div style=width:{width}px>lead ");
            let mut rows = Vec::new();
            let mut ids = 0;
            while ids == 0 {
                table_content(&mut next, 0, &mut markup, &mut rows, &mut ids);
            }
            markup.push_str("tail</div>");
            let laid = laid(&markup);
            let (first, last) = page_bounds(&laid);
            let copied = copy_of(&laid, first, last);
            for (_, row) in &rows {
                let mut cells = row
                    .iter()
                    .enumerate()
                    .filter_map(|(at, id)| Some((at, (*id)?)));
                let Some(mut before) = cells.next() else {
                    continue;
                };
                for after in cells {
                    let tabs = "\t".repeat(after.0 - before.0);
                    let needle = format!("e{}{tabs}s{}", before.1, after.1);
                    assert!(
                        holds_token(&copied, &needle),
                        "{page}: {markup}: {needle:?} in {copied:?}"
                    );
                    checked += 1;
                    before = after;
                }
            }
            // A table's rows, after any of a nested table's pushed between.
            for (at, (table, row)) in rows.iter().enumerate() {
                let Some(a) = row.iter().rev().find_map(|id| *id) else {
                    continue;
                };
                let Some((_, below)) = rows.iter().skip(at + 1).find(|(t, _)| t == table) else {
                    continue;
                };
                if let Some(b) = below.iter().find_map(|id| *id) {
                    let needle = format!("e{a}\ns{b}");
                    assert!(
                        holds_token(&copied, &needle),
                        "{page}: {markup}: {needle:?} in {copied:?}"
                    );
                    checked += 1;
                }
            }
            let mut scalars = Vec::new();
            let mut spans: Vec<(Place, Place)> = Vec::new();
            let _ = walk(&laid, None, 0, |event| {
                match event {
                    Event::Scalar(c, s, e, _) => {
                        scalars.push((c, s));
                        spans.push((s, e));
                    }
                    Event::Alt(_, s, e) | Event::Gap(_, s, e) => spans.push((s, e)),
                    _ => {}
                }
                ControlFlow::Continue(())
            });
            assert!(
                spans
                    .windows(2)
                    .all(|w| w[0].0 < w[1].0 && w[0].1 <= w[1].0),
                "{page}: {markup}: not in tree order"
            );
            let mut places = vec![first, last];
            for line in laid.lines() {
                for item in items(&laid, line) {
                    places.push(item.start);
                    places.push(item.end);
                }
            }
            places.sort();
            places.dedup();
            for _ in 0..40 {
                let a = places[next(places.len() as u64) as usize];
                let b = places[next(places.len() as u64) as usize];
                let (from, to) = (a.min(b), a.max(b));
                let shown: String = scalars
                    .iter()
                    .filter(|(c, s)| *s >= from && *s < to && !c.is_whitespace())
                    .map(|(c, _)| *c)
                    .collect();
                let mut out = copy_of(&laid, from, to);
                out.retain(|c| !c.is_whitespace());
                assert_eq!(out, shown, "{page}: {markup} {from:?} {to:?}");
            }
            for _ in 0..12 {
                let at = places[next(places.len() as u64) as usize];
                let ahead = spans.iter().any(|&(_, e)| e > at);
                let behind = spans.iter().any(|&(s, _)| s < at);
                for motion in [Move::Right, Move::WordRight, Move::Down] {
                    let to = moved(&laid, &mut m, at, motion);
                    assert!(
                        to > at || to == at && (motion != Move::Right || !ahead),
                        "{page}: {markup}: {motion:?} from {at:?} to {to:?}"
                    );
                }
                for motion in [Move::Left, Move::WordLeft, Move::Up] {
                    let to = moved(&laid, &mut m, at, motion);
                    assert!(
                        to < at || to == at && (motion != Move::Left || !behind),
                        "{page}: {markup}: {motion:?} from {at:?} to {to:?}"
                    );
                }
                // Left goes to the unit before: its start, or its end
                // where a paragraph or a box starts between.
                if let Some(&(s, e)) = spans.iter().rev().find(|&&(s, _)| s < at) {
                    let to = moved(&laid, &mut m, at, Move::Left);
                    assert!(
                        to == s || to == e,
                        "{page}: {markup}: Left from {at:?} to {to:?}"
                    );
                }
            }
        }
        assert!(checked > 200, "{checked}");
    }
}
