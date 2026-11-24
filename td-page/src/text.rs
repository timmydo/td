//! Places in a laid page and the text between them (td-web/DESIGN.md §10,
//! "Places, find, selection and links"): a place is a DOM position, a
//! paragraph a run of lines one block lays up to a line ended by a
//! newline, a `br` or its block (`Line::end`), and the text find searches
//! and a copy takes is the fragments' displayed text with the white space
//! a `Wrap` leaves between lines put back.

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
use td_layout::{StyleId, TableRole};

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

/// The first and last lines of the paragraph line `at` lies in.
pub fn paragraph(laid: &Laid, at: usize) -> (usize, usize) {
    let lines = laid.lines();
    let block = lines.get(at).and_then(Line::block);
    let mut first = at;
    while let Some(before) = first.checked_sub(1).and_then(|b| lines.get(b)) {
        if ends_paragraph(before) || before.block() != block {
            break;
        }
        first -= 1;
    }
    let mut last = at;
    while let Some(line) = lines.get(last) {
        if ends_paragraph(line) || lines.get(last + 1).is_none_or(|next| next.block() != block) {
            break;
        }
        last += 1;
    }
    (first, last.min(lines.len().saturating_sub(1)))
}

/// Each line's enclosing line (`Laid::outer`), `NONE` for none: lines are
/// in the tree order of their starts, and the lines of a float met within
/// a line, or of an inline-block in it, follow it, starting and ending
/// within it. A line with no item takes the next line's with one, as it
/// takes its start; those ending the page none. Each line is set once.
pub(crate) fn nesting(laid: &Laid, memory: &Budget) -> Result<BudgetVec<u32>, BudgetError> {
    let lines = laid.lines();
    let mut outer = td_alloc::BudgetVec::with_capacity(memory, lines.len())?;
    let mut open: BudgetVec<(u32, Place)> = BudgetVec::new(memory)?;
    // Item-less lines waiting for the next line with an item, whose
    // enclosing line they take, as they take its start (`line_of`).
    let mut waiting = 0;
    for (at, line) in lines.iter().enumerate() {
        let Some((start, end)) = bounds(laid, line) else {
            outer.try_push(NONE)?;
            continue;
        };
        while open.last().is_some_and(|&(_, end)| end <= start) {
            open.pop();
        }
        let within = open.last().map_or(NONE, |&(line, _)| line);
        for slot in outer.get_mut(waiting..).unwrap_or(&mut []) {
            *slot = within;
        }
        outer.try_push(within)?;
        waiting = outer.len();
        open.try_push((u32::try_from(at).unwrap_or(NONE), end))?;
    }
    Ok(outer)
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
    // The line after the one climbed from: a sibling of `at` where its
    // float's lines go on.
    let next = at + 1;
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
        if !held && laid.outer(next) == laid.outer(at) {
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
/// collapse, the run where they are kept, none at an edge.
fn gap<'a>(laid: &'a Laid, a: &Line, b: &Line) -> Option<(&'a str, Place, Place)> {
    let last = shown_items(laid, a).last()?;
    let next = shown_items(laid, b).next()?;
    let (Piece::Text(i), Piece::Text(j)) = (last.piece, next.piece) else {
        return None;
    };
    let fa = a.fragments.get(i)?;
    let fb = b.fragments.get(j)?;
    let document = laid.tree().document();
    let tail = document.node(fa.node)?.text()?.get(fa.range.end..)?;
    let head = document.node(fb.node)?.text()?.get(..fb.range.start)?;
    let between: &str = if fa.node == fb.node {
        tail.get(..fb.range.start.checked_sub(fa.range.end)?)?
    } else if tail.trim().is_empty() && !tail.is_empty() {
        tail
    } else {
        head
    };
    // Past a cut only what was dropped as space.
    let space = between.trim_start_matches(|c: char| !c.is_whitespace() || c == '\u{a0}');
    if space.is_empty() {
        return None;
    }
    let shown = if preserves(laid, fa) { space } else { " " };
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
    /// A float's or an inline-block's lines lie here, within the line
    /// walked, or an inline-block's placeholder: find matches nothing
    /// across it.
    Break,
    /// The paragraph ends.
    End,
}

/// The first place of each line lying directly within line `at`, in
/// order: where the floats and inline-blocks met in it lie between its
/// pieces. The lines within it, at any depth, follow it as one run,
/// each within it or within one of that run before it (`nesting`), a
/// line with no item (a `<br>`'s) within the next line's.
fn inner_starts(laid: &Laid, at: usize) -> impl Iterator<Item = Place> + '_ {
    laid.lines()
        .iter()
        .enumerate()
        .skip(at + 1)
        .take_while(move |&(line, _)| laid.outer(line).is_some_and(|outer| outer >= at))
        .filter(move |&(line, _)| laid.outer(line) == Some(at))
        .filter_map(move |(_, line)| bounds(laid, line).map(|(start, _)| start))
}

/// Each event of lines `from..` in order, until `each` breaks; hidden
/// pieces give none. With the layout's measure each scalar's advance is
/// replayed.
pub fn walk<'a>(
    laid: &'a Laid,
    measure: Option<&mut Measure>,
    from: usize,
    each: impl FnMut(Event<'a>) -> ControlFlow<()>,
) -> ControlFlow<()> {
    walk_to(laid, measure, from, laid.lines().len(), each)
}

/// As `walk`, of lines `from..until`.
fn walk_to<'a>(
    laid: &'a Laid,
    mut measure: Option<&mut Measure>,
    from: usize,
    until: usize,
    mut each: impl FnMut(Event<'a>) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let lines = laid.lines();
    let mut begun = false;
    let mut wrapped: Option<&Line> = None;
    for (at, line) in lines.iter().enumerate().take(until).skip(from) {
        if !begun {
            each(Event::Begin(at))?;
            begun = true;
        }
        let laid_any = shown_items(laid, line).next().is_some();
        if laid_any {
            if let Some(before) = wrapped.take() {
                if let Some((text, start, end)) = gap(laid, before, line) {
                    each(Event::Gap(text, start, end))?;
                }
            }
        }
        let mut inner = inner_starts(laid, at).peekable();
        for item in shown_items(laid, line) {
            let mut broken = false;
            while inner.next_if(|start| *start < item.start).is_some() {
                broken = true;
            }
            if broken {
                each(Event::Break)?;
            }
            match item.piece {
                Piece::Text(i) => {
                    let Some(f) = line.fragments.get(i) else {
                        continue;
                    };
                    if f.display == DisplayText::Space {
                        each(Event::Scalar(' ', item.start, item.end, false))?;
                        continue;
                    }
                    if let Some(measure) = measure.as_deref_mut() {
                        let mut flow = ControlFlow::Continue(());
                        let mut reported = false;
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
                            continue;
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
                        each(Event::Scalar(shown, start, end, false))?;
                    }
                }
                Piece::Replaced(i) => {
                    let Some(r) = line.replaced.get(i) else {
                        continue;
                    };
                    // An inline-block's text is its own lines'.
                    if r.atomic {
                        each(Event::Break)?;
                        continue;
                    }
                    let alt = laid.tree().alt(r.node).unwrap_or("");
                    each(Event::Alt(alt, item.start, item.end))?;
                }
            }
        }
        match line.end() {
            LineEnd::Wrap if laid_any => wrapped = Some(line),
            LineEnd::Wrap => {}
            _ => wrapped = None,
        }
        let block_ends = lines
            .get(at + 1)
            .is_none_or(|next| next.block() != line.block());
        if ends_paragraph(line) || block_ends {
            wrapped = None;
            begun = false;
            each(Event::End)?;
        }
    }
    ControlFlow::Continue(())
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

/// Each unit a caret steps over from line `from` on, with whether a
/// paragraph ended since the unit before it: a scalar with the marks of
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
            Event::Begin(_) | Event::Break => return ControlFlow::Continue(()),
            Event::End => {
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
    let line = line_of(laid, at);
    let (first, last) = paragraph(laid, line);
    let before = paragraph(laid, first.saturating_sub(1)).0;
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
            let mut previous: Option<(Place, Place)> = None;
            units(laid, measure, before, |_, s, e, crossed| {
                if s >= at {
                    // At a paragraph's start: the end of the one before.
                    if crossed && s == at {
                        found = previous.map(|(_, pe)| pe);
                    }
                    return ControlFlow::Break(());
                }
                previous = Some((s, e));
                found = Some(s);
                ControlFlow::Continue(())
            });
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
            let mut previous_word = false;
            units(laid, measure, before, |c, s, _, crossed| {
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
        }
        Move::Up => {
            let start = paragraph_places(laid, at).map(|(start, _)| start);
            found = match start {
                Some(start) if start < at => Some(start),
                _ if first > 0 => paragraph_places_of_line(laid, first - 1).map(|(s, _)| s),
                _ => None,
            };
        }
        Move::Down => {
            let end = paragraph_places(laid, at).map(|(_, end)| end);
            found = match end {
                Some(end) if end > at => Some(end),
                _ => {
                    let mut next = last + 1;
                    let mut after = None;
                    while next < laid.lines().len() {
                        if let Some((_, end)) = paragraph_places_of_line(laid, next) {
                            after = Some(end);
                            break;
                        }
                        next = paragraph(laid, next).1 + 1;
                    }
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
/// and `_`, each with the marks of no advance after it.
pub fn word(laid: &Laid, measure: &mut Measure, at: Place) -> Option<(Place, Place)> {
    let line = line_of(laid, at);
    let (first, _) = paragraph(laid, line);
    let mut run: Option<(Place, Place)> = None;
    let mut found = None;
    units(laid, measure, first, |c, s, e, crossed| {
        if crossed {
            return ControlFlow::Break(());
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
        ControlFlow::Continue(())
    });
    // The paragraph's last run holds its end too.
    found.or(run.filter(|run| run.0 <= at && at <= run.1))
}

/// The paragraph line `line` lies in, start to end.
pub fn paragraph_places_of_line(laid: &Laid, line: usize) -> Option<(Place, Place)> {
    let (first, last) = paragraph(laid, line);
    let lines = laid.lines().get(first..=last)?;
    let start = lines.iter().find_map(|l| bounds(laid, l))?;
    let end = lines.iter().rev().find_map(|l| bounds(laid, l))?;
    Some((start.0, end.1))
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
}

#[derive(Clone, Copy)]
enum Owed {
    Newline,
    Space,
    Tabs(usize),
}

impl Copier<'_> {
    fn put(&mut self, text: &str) -> ControlFlow<()> {
        if self.started {
            match self.owed.take() {
                Some(Owed::Newline) => (self.out)("\n")?,
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
        (self.out)(text)
    }
    /// The copy ends at the next piece: a selection ending at a
    /// paragraph's start holds the newline before it.
    fn close(&mut self, at_start: bool) -> ControlFlow<()> {
        if at_start && self.started && matches!(self.owed, Some(Owed::Newline)) {
            (self.out)("\n")?;
        }
        ControlFlow::Break(())
    }
    fn owe(&mut self, owed: Owed) {
        self.owed = Some(match (self.owed, owed) {
            (Some(Owed::Newline), _) | (_, Owed::Newline) => Owed::Newline,
            (Some(Owed::Tabs(a)), Owed::Tabs(b)) => Owed::Tabs(a + b),
            (Some(Owed::Tabs(a)), Owed::Space) => Owed::Tabs(a),
            (_, owed) => owed,
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
    // `end`, in line order: from the outermost line the start's lies in,
    // whose tail after a float may hold some, to the last line starting
    // at or before the end, whose markers a line starting there holds.
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
    };
    let mut cell: Option<(usize, Option<usize>)> = None;
    let mut buffer = [0u8; 4];
    let markers = &laid.layout.markers;
    let mut marker = 0usize;
    let (first, last) = page_bounds(laid);
    let whole = start <= first && end >= last;
    let (top, bottom) = ((start, whole), (end, whole));
    // What was owed before the last paragraph end walked: a line starting
    // at the end owes nothing for its own.
    let mut ended: Option<Option<Owed>> = None;
    let flow = walk_to(laid, None, from, until, |event| match event {
        Event::Begin(at) => {
            ended = None;
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
                    Some(l) if l as usize == at && held && !hidden(laid, m.style) => {
                        // Its text ends in its own space.
                        copier.put(m.text().as_str())?;
                    }
                    // An empty item's, before this line, is a paragraph
                    // of its own when the selection spans its item.
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
        Event::Scalar(c, s, e, _) => {
            if s >= start && e <= end {
                copier.put(c.encode_utf8(&mut buffer))?;
            }
            ControlFlow::Continue(())
        }
        Event::Alt(text, s, e) => {
            if s >= start && e <= end {
                copier.put(text)?;
            }
            ControlFlow::Continue(())
        }
        Event::Gap(text, s, e) => {
            if s >= start && e <= end && copier.started {
                copier.put(text)?;
            }
            ControlFlow::Continue(())
        }
        Event::Break => ControlFlow::Continue(()),
        Event::End => {
            ended = Some(copier.owed);
            if copier.started {
                copier.owe(if cell.is_some() {
                    Owed::Space
                } else {
                    Owed::Newline
                });
            }
            ControlFlow::Continue(())
        }
    });
    flow?;
    // Ended before the page's end; at a line's start, with the newline
    // before it.
    if until < lines.len() || at_start {
        if let (true, Some(owed)) = (at_start, ended) {
            copier.owed = owed;
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
    /// within the line its next line does; those ending the page in none.
    #[test]
    fn item_less_lines_lie_where_the_next_line_does() {
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
    /// from the start to before the end, in line order.
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
    /// scalars painted selected in line order, an inline-block's text
    /// after its line's, the page's end is past its text, and its
    /// placeholder gives no `alt`.
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
        assert_eq!(copy_of(&laid, first, last), "one  two\naa bb\ncc");
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
}
