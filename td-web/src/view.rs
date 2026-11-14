//! Where the reader is in a rendered page: where the first line shown
//! starts, the focused link and what find matched; and the page painted
//! over its pane in prose, each glyph at the pen its line was laid with.

use std::collections::VecDeque;

use td_ui::prose::{self, LineBox, Metrics, OVERHANG, SIZE};
use td_ui::raster::{
    fill, Draw, GlyphStyle, Primitive, Rect, ACCENT, CHROME, INACTIVE_SELECTION, INK, PAPER,
    SELECTED,
};

use crate::document::{Place, Rendered, Wrapped, NO_LINK};

/// The most matches find marks.
pub const MAX_HITS: usize = 10_000;
/// The most glyphs drawn at one pen: combining marks take no advance in
/// Noto, so without a bound one line could ask a paint for as many glyphs
/// as the page has characters.
const MAX_STACKED: usize = 8;

/// A match: where its first character is in the page's logical lines, and
/// where the text after its last is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hit {
    pub start: Place,
    pub end: Place,
}

/// Where a page is shown: its pane, the inset left of its lines, the
/// line box they are laid in and the scale.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pane {
    pub rect: Rect,
    pub inset: i64,
    pub line: LineBox,
    pub scale: i64,
}

impl Pane {
    fn height(&self) -> i64 {
        i64::from(self.line.height.max(1))
    }

    /// The lines shown whole.
    pub fn shown(&self) -> usize {
        usize::try_from(i64::from(self.rect.height) / self.height()).unwrap_or(0)
    }

    /// The lines shown whole, at least one, as scrolling counts them.
    pub fn rows(&self) -> usize {
        self.shown().max(1)
    }
}

/// Text selected in the page: where it was begun and where it is now,
/// in either order, as places in its logical lines, so it is kept however
/// the page is wrapped and however little of it is laid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub anchor: Place,
    pub focus: Place,
}

impl Selection {
    /// Its ends in the page's order; none when it holds nothing.
    pub fn range(self) -> Option<(Place, Place)> {
        let (start, end) = if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        };
        (start < end).then_some((start, end))
    }
}

/// Where a point of the pane falls in the page's text: the place between
/// characters nearest it (`caret`), and the character under it, if any.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Spot {
    pub caret: Place,
    pub under: Option<Place>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct View {
    /// Where the first line shown starts.
    pub top: Place,
    /// The focused link.
    pub focus: Option<usize>,
    /// Every match, in the page's order.
    pub hits: Vec<Hit>,
    /// The current match, an index into `hits`.
    pub at: usize,
    /// The text selected, which find's current match becomes.
    pub selection: Option<Selection>,
}

/// The first of `count` items for which `before` is false, `before` being
/// true of a first stretch of them and false of the rest.
fn first_not(count: usize, before: impl Fn(usize) -> bool) -> usize {
    let (mut low, mut high) = (0, count);
    while low < high {
        let middle = low + (high - low) / 2;
        if before(middle) {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

impl View {
    /// A navigation action may partially fill a cache, but a refused action
    /// keeps the logical viewport, focus and selection. Painting reports it;
    /// the next action retries after any pressure has been released.
    fn navigate(&mut self, rendered: &mut Rendered, action: impl FnOnce(&mut Self, &mut Rendered)) {
        let previous = (self.top, self.focus, self.at, self.selection);
        rendered.retry();
        action(self, rendered);
        if rendered.refused() {
            (self.top, self.focus, self.at, self.selection) = previous;
            self.at = self.at.min(self.hits.len().saturating_sub(1));
        }
    }
    #[cfg(test)]
    pub fn settle(&mut self, rendered: &mut Rendered, metrics: &mut Metrics, rows: usize) {
        self.navigate(rendered, |this, rendered| {
            this.settle_inner(rendered, metrics, rows)
        });
    }
    pub fn scroll(
        &mut self,
        delta: isize,
        rendered: &mut Rendered,
        metrics: &mut Metrics,
        rows: usize,
    ) {
        self.navigate(rendered, |this, rendered| {
            this.scroll_inner(delta, rendered, metrics, rows)
        });
    }
    pub fn home(&mut self, rendered: &mut Rendered, metrics: &mut Metrics, rows: usize) {
        self.navigate(rendered, |this, rendered| {
            this.home_inner(rendered, metrics, rows)
        });
    }
    pub fn end(&mut self, rendered: &mut Rendered, metrics: &mut Metrics, rows: usize) {
        self.navigate(rendered, |this, rendered| {
            this.end_inner(rendered, metrics, rows)
        });
    }
    pub fn reveal(
        &mut self,
        place: Place,
        rendered: &mut Rendered,
        metrics: &mut Metrics,
        rows: usize,
    ) {
        self.navigate(rendered, |this, rendered| {
            this.reveal_inner(place, rendered, metrics, rows)
        });
    }
    pub fn step_link(
        &mut self,
        forward: bool,
        rendered: &mut Rendered,
        metrics: &mut Metrics,
        rows: usize,
    ) {
        self.navigate(rendered, |this, rendered| {
            this.step_link_inner(forward, rendered, metrics, rows)
        });
    }
    pub fn find(
        &mut self,
        query: &str,
        rendered: &mut Rendered,
        metrics: &mut Metrics,
        rows: usize,
    ) {
        self.navigate(rendered, |this, rendered| {
            this.find_inner(query, rendered, metrics, rows)
        });
    }
    pub fn step_hit(
        &mut self,
        forward: bool,
        rendered: &mut Rendered,
        metrics: &mut Metrics,
        rows: usize,
    ) {
        self.navigate(rendered, |this, rendered| {
            this.step_hit_inner(forward, rendered, metrics, rows)
        });
    }

    /// Selects `selection`, or nothing. A selection and a focused link
    /// exclude each other, so a copy takes the one there is: selecting
    /// lets the focused link go, and focusing a link clears the selection.
    pub fn select(&mut self, selection: Option<Selection>) {
        if selection.is_some() {
            self.focus = None;
        }
        self.selection = selection;
    }

    /// Keeps the view inside the page, its top no further down than the
    /// last screenful's first line, and lays the lines shown.
    fn settle_inner(&mut self, rendered: &mut Rendered, metrics: &mut Metrics, rows: usize) {
        let rows = rows.max(1);
        let (_, below) = rendered.forward(metrics, self.top, rows - 1);
        if below < rows - 1 {
            self.top = rendered.back(metrics, self.top, rows - 1 - below).0;
        }
        rendered.settle(metrics, self.top, rows);
    }

    /// Moves `delta` lines, kept inside the page.
    fn scroll_inner(
        &mut self,
        delta: isize,
        rendered: &mut Rendered,
        metrics: &mut Metrics,
        rows: usize,
    ) {
        let steps = delta.unsigned_abs();
        self.top = if delta < 0 {
            rendered.back(metrics, self.top, steps).0
        } else {
            rendered.forward(metrics, self.top, steps).0
        };
        self.settle_inner(rendered, metrics, rows);
    }

    fn home_inner(&mut self, rendered: &mut Rendered, metrics: &mut Metrics, rows: usize) {
        self.top = Place::default();
        self.settle_inner(rendered, metrics, rows);
    }

    /// The last screenful, laid back from the page's last logical line.
    fn end_inner(&mut self, rendered: &mut Rendered, metrics: &mut Metrics, rows: usize) {
        let last = rendered.last(metrics);
        self.top = rendered.back(metrics, last, rows.max(1) - 1).0;
        self.settle_inner(rendered, metrics, rows);
    }

    /// Scrolls as little as shows the line holding `place`.
    fn reveal_inner(
        &mut self,
        place: Place,
        rendered: &mut Rendered,
        metrics: &mut Metrics,
        rows: usize,
    ) {
        let rows = rows.max(1);
        let line = rendered.snap(metrics, place);
        if line < self.top {
            self.top = line;
        } else if line > rendered.forward(metrics, self.top, rows - 1).0 {
            self.top = rendered.back(metrics, line, rows - 1).0;
        }
        self.settle_inner(rendered, metrics, rows);
    }

    /// Focuses the next link (`forward`) or the previous one: from the
    /// focused link, else from the screen, the first starting at or after
    /// its top or the last starting before its bottom.
    fn step_link_inner(
        &mut self,
        forward: bool,
        rendered: &mut Rendered,
        metrics: &mut Metrics,
        rows: usize,
    ) {
        let Ok(page) = rendered.page.try_clone() else {
            return;
        };
        let count = page.links.len();
        self.selection = None;
        if count == 0 {
            self.focus = None;
            return;
        }
        let place = |at| page.link_place(at).unwrap_or_default();
        let next = match self.focus {
            Some(at) if forward => (at + 1) % count,
            Some(at) => (at + count - 1) % count,
            None if forward => {
                let first = first_not(count, |at| place(at) < self.top);
                if first < count {
                    first
                } else {
                    0
                }
            }
            None => {
                // The line after the last shown, unless the page ends first.
                let (bottom, moved) = rendered.forward(metrics, self.top, rows.max(1));
                let below = if moved < rows.max(1) {
                    count
                } else {
                    first_not(count, |at| place(at) < bottom)
                };
                below.checked_sub(1).unwrap_or(count - 1)
            }
        };
        self.focus = Some(next);
        self.reveal_inner(place(next), rendered, metrics, rows);
    }

    /// Marks every match of `query` (`mark`) and makes current the first
    /// at or after the screen's top, scrolling to it.
    fn find_inner(
        &mut self,
        query: &str,
        rendered: &mut Rendered,
        metrics: &mut Metrics,
        rows: usize,
    ) {
        self.mark(query, rendered, metrics);
        let top = self.top;
        let first = self.hits.partition_point(|hit| hit.start < top);
        self.at = if first < self.hits.len() { first } else { 0 };
        if self.hits.is_empty() {
            self.selection = None;
        }
        self.show_hit(rendered, metrics, rows);
    }

    /// Marks every match of `query` again on lines wrapped anew, keeping
    /// which is current as far as there are as many, and not scrolling.
    pub fn remark(&mut self, query: &str, rendered: &Rendered, metrics: &mut Metrics) {
        let at = self.at;
        self.mark(query, rendered, metrics);
        self.at = at.min(self.hits.len().saturating_sub(1));
    }

    /// Marks every match of `query` in each logical line, across its runs
    /// and its wrapped lines and ignoring case, at most `MAX_HITS`: all of
    /// a line that wraps, and only what is laid of one that does not
    /// (`Rendered::searched`), the query's characters and the page's
    /// alike as they are shown, a tab as a space. Matches do not overlap,
    /// the first found first. Each character is looked at once (Knuth,
    /// Morris and Pratt), so a search costs the page's text whatever the
    /// query.
    fn mark(&mut self, query: &str, rendered: &Rendered, metrics: &mut Metrics) {
        self.hits.clear();
        self.at = 0;
        let needle: Vec<char> = query.chars().map(|c| fold(prose::shown(c))).collect();
        if needle.is_empty() {
            return;
        }
        let borders = borders(&needle);
        let hits = &mut self.hits;
        // Where the characters last matched are.
        let mut window: VecDeque<Place> = VecDeque::with_capacity(needle.len());
        let mut full = false;
        for logical in 0..rendered.page.lines.len() {
            window.clear();
            let mut matched = 0;
            rendered.searched(metrics, logical, |scalar, place| {
                let seen = fold(prose::shown(scalar));
                while matched > 0 && needle.get(matched) != Some(&seen) {
                    matched = borders.get(matched - 1).copied().unwrap_or(0);
                }
                if needle.get(matched) == Some(&seen) {
                    matched += 1;
                }
                window.push_back(place);
                while window.len() > matched {
                    window.pop_front();
                }
                if matched < needle.len() {
                    return true;
                }
                if hits.len() == MAX_HITS {
                    full = true;
                    return false;
                }
                let start = window.front().copied().unwrap_or(place);
                let end = Place {
                    at: place.at + scalar.len_utf8(),
                    ..place
                };
                hits.push(Hit { start, end });
                window.clear();
                matched = 0;
                true
            });
            if full {
                return;
            }
        }
    }

    /// Moves to the next match or the previous one, around the ends.
    fn step_hit_inner(
        &mut self,
        forward: bool,
        rendered: &mut Rendered,
        metrics: &mut Metrics,
        rows: usize,
    ) {
        let count = self.hits.len();
        if count == 0 {
            return;
        }
        self.at = if forward {
            (self.at + 1) % count
        } else {
            (self.at + count - 1) % count
        };
        self.show_hit(rendered, metrics, rows);
    }

    /// Shows the current match whole where the screen holds it: its last
    /// character's line, then its first drawn's, a space a wrapped line
    /// ends with being dropped there; and selects it.
    fn show_hit(&mut self, rendered: &mut Rendered, metrics: &mut Metrics, rows: usize) {
        if let Some(hit) = self.hits.get(self.at).copied() {
            self.select(Some(Selection {
                anchor: hit.start,
                focus: hit.end,
            }));
            let last = Place {
                at: hit.end.at.saturating_sub(1),
                ..hit.end
            };
            self.reveal_inner(last, rendered, metrics, rows);
            let first = rendered.page.drawn(hit.start, hit.end);
            self.reveal_inner(first, rendered, metrics, rows);
        }
    }

    /// Where a point of `pane` falls in the text of the lines shown: on a
    /// line, the place between characters nearest it, by the halves of
    /// the glyphs drawn there, and the character under it; left of a
    /// line's text, its own start (a continuation's repeated marks are
    /// not page text); right of it, or in the inset right of the lines,
    /// its end, which for a line that is not wrapped is its logical
    /// line's, past what is laid of it. Above the pane, the first line
    /// shown's start; below the last line shown whole, the last's end.
    /// No place is before a glyph of no width that follows one with
    /// width, so a mark stays with its base. None with no line shown
    /// whole.
    pub fn spot(&self, rendered: &Rendered, pane: &Pane, x: i64, y: i64) -> Option<Spot> {
        if rendered.refused() {
            return None;
        }
        let rect = pane.rect;
        let shown = || rendered.shown(self.top, pane.shown());
        let first = shown().next()?;
        let end = |wrapped: &Wrapped| Spot {
            caret: line_end(rendered, wrapped),
            under: None,
        };
        if y < rect.y {
            return Some(Spot {
                caret: first.place(),
                under: None,
            });
        }
        let row = usize::try_from((y - rect.y) / pane.height()).unwrap_or(usize::MAX);
        let Some(wrapped) = shown().nth(row) else {
            return shown().last().map(end);
        };
        let right = rect.x.saturating_add(i64::from(rect.width));
        if x >= right.saturating_sub(pane.inset) {
            return Some(end(wrapped));
        }
        let x = x - rect.x.saturating_add(pane.inset);
        let mut under = None;
        // Whether a glyph with width came before: a base marks follow.
        let mut based = false;
        for (place, left, right) in glyphs(rendered, wrapped) {
            let (left, right) = (i64::from(left), i64::from(right));
            if left <= x && x < right {
                under = Some(place);
            }
            // A place falls before a glyph of no width, a combining mark,
            // only where no base comes before it, for a point at or left
            // of it: a mark is never parted from its base, and a line that
            // begins with marks still has a start.
            let before = if left < right {
                x < left + (right - left) / 2
            } else {
                !based && x <= left
            };
            based |= left < right;
            if before {
                return Some(Spot {
                    caret: place,
                    under,
                });
            }
        }
        Some(Spot {
            under,
            ..end(wrapped)
        })
    }

    /// The link painted at a point of `pane`: none in the inset left of
    /// the lines, and none on the line below the last shown whole.
    pub fn link_at(&self, rendered: &Rendered, pane: &Pane, x: i64, y: i64) -> Option<usize> {
        if rendered.refused() {
            return None;
        }
        let rect = pane.rect;
        let left = rect.x.saturating_add(pane.inset);
        if !rect.contains(x, y) || x < left {
            return None;
        }
        let row = usize::try_from((y - rect.y) / pane.height()).ok()?;
        if row >= pane.shown() {
            return None;
        }
        let wrapped = rendered.shown(self.top, pane.shown()).nth(row)?;
        let piece = wrapped.line.hit(x - left)?;
        (piece.tag < rendered.page.links.len()).then_some(piece.tag)
    }
}

/// The glyphs of `wrapped` that show the page's text, in order: where
/// each one's character is, and the pixels it spans from the line's left.
/// A continuation's repeated marks are left out.
fn glyphs<'a>(
    rendered: &'a Rendered,
    wrapped: &'a Wrapped,
) -> impl Iterator<Item = (Place, u32, u32)> + 'a {
    let line = &wrapped.line;
    line.pieces
        .iter()
        .filter(|piece| wrapped.shows(piece))
        .flat_map(move |piece| {
            let text = rendered
                .page
                .run(wrapped, piece)
                .and_then(|run| run.text.get(piece.text.clone()))
                .unwrap_or_default();
            text.char_indices()
                .zip(piece.glyphs.clone())
                .filter_map(move |((byte, _), at)| {
                    let glyph = line.glyphs.get(at)?;
                    let right = line
                        .glyphs
                        .get(at + 1)
                        .map_or(line.width, |next| next.x)
                        .max(glyph.x);
                    let place = Place {
                        logical: wrapped.logical,
                        run: piece.run,
                        at: piece.text.start + byte,
                    };
                    Some((place, glyph.x, right))
                })
        })
}

/// Where `wrapped`'s text ends: after its last glyph, its own start when
/// it shows none; for a line that is not wrapped, its logical line's end,
/// past what is laid of it.
fn line_end(rendered: &Rendered, wrapped: &Wrapped) -> Place {
    let unwrapped = rendered
        .page
        .lines
        .get(wrapped.logical)
        .is_some_and(|line| !line.wrap);
    if unwrapped {
        return rendered.page.line_end(wrapped.logical);
    }
    let last = glyphs(rendered, wrapped).last();
    last.and_then(|(place, _, _)| {
        let c = rendered
            .page
            .lines
            .get(place.logical)?
            .runs
            .get(place.run)?
            .text
            .get(place.at..)?
            .chars()
            .next()?;
        Some(place.past(c))
    })
    .unwrap_or_else(|| wrapped.place())
}

/// For each prefix of `needle`, how long its longest prefix that is also
/// its suffix, and not all of it, is: where a match that fails after it
/// goes on from.
fn borders(needle: &[char]) -> Vec<usize> {
    let mut table = vec![0; needle.len()];
    let mut k = 0;
    for i in 1..needle.len() {
        while k > 0 && needle.get(i) != needle.get(k) {
            k = table.get(k - 1).copied().unwrap_or(0);
        }
        if needle.get(i) == needle.get(k) {
            k += 1;
        }
        if let Some(slot) = table.get_mut(i) {
            *slot = k;
        }
    }
    table
}

/// One character as find compares it.
fn fold(c: char) -> char {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

/// What a pass over a line paints: the grounds under its glyphs, the
/// glyphs over them, then the strokes, so no ground covers ink a
/// neighbouring glyph hangs over it.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Pass {
    Ground,
    Glyph,
    Stroke,
}

/// Paints `rendered` as `view` shows it inside `pane`, its lines the
/// pane's inset from its left; `hover` is the link under the pointer. A
/// line wider than the pane, a preformatted one, is cut at its edge.
/// The lines shown are those laid (`View::settle`).
pub fn paint(
    rendered: &Rendered,
    view: &View,
    hover: Option<usize>,
    pane: &Pane,
    sink: &mut dyn FnMut(Draw),
) {
    let rect = pane.rect;
    fill(rect, PAPER, rect, sink);
    let s = pane.scale;
    let height = pane.height();
    let baseline = i64::from(pane.line.baseline);
    // About half an x-height above the baseline: 9 of the cell's 16 rows.
    let strike = baseline - i64::from(SIZE) * s * 5 / 16;
    let left = rect.x.saturating_add(pane.inset);
    // Past this a glyph inks nothing inside the pane, even one whose ink
    // hangs left of its pen (`prose::OVERHANG`).
    let edge = rect
        .x
        .saturating_add(i64::from(rect.width))
        .saturating_add(i64::from(OVERHANG));
    let lines = rendered.shown(view.top, pane.shown());
    for (row, wrapped) in lines.enumerate() {
        let y = rect.y + row as i64 * height;
        // The line's matches: `hits` is in the page's order, and so are
        // the characters a line shows.
        let from = wrapped.place();
        let first = view.hits.partition_point(|hit| hit.end <= from);
        let selected = view.selection.and_then(Selection::range);
        let line = &wrapped.line;
        for pass in [Pass::Ground, Pass::Glyph, Pass::Stroke] {
            // The first match not wholly before the glyph.
            let mut next = first;
            'pieces: for piece in &line.pieces {
                let Some(run) = rendered.page.run(wrapped, piece) else {
                    continue;
                };
                let link = (piece.tag != NO_LINK).then_some(piece.tag);
                let focused = link.is_some() && link == view.focus;
                let hovered = link.is_some() && link == hover;
                // Where each glyph's character is, unless the piece is
                // marks a continuation repeats.
                let shows = wrapped.shows(piece);
                let mut chars = run
                    .text
                    .get(piece.text.clone())
                    .unwrap_or_default()
                    .char_indices();
                for at in piece.glyphs.clone() {
                    let place = chars.next().filter(|_| shows).map(|(byte, _)| Place {
                        logical: wrapped.logical,
                        run: piece.run,
                        at: piece.text.start + byte,
                    });
                    let Some(glyph) = line.glyphs.get(at) else {
                        break;
                    };
                    let x = left.saturating_add(i64::from(glyph.x));
                    if x >= edge {
                        break 'pieces;
                    }
                    // Pens never go back, so this glyph and the
                    // `MAX_STACKED` before it share one pen: marks of no
                    // advance, which a hostile page can pile by the
                    // million. The rest of the pile is not drawn.
                    let stacked = at
                        .checked_sub(MAX_STACKED)
                        .and_then(|back| line.glyphs.get(back))
                        .is_some_and(|earlier| earlier.x == glyph.x);
                    if stacked {
                        continue;
                    }
                    let right = line
                        .glyphs
                        .get(at + 1)
                        .map_or(line.width, |next| next.x)
                        .max(glyph.x);
                    let hit = place.and_then(|place| {
                        while view.hits.get(next).is_some_and(|hit| hit.end <= place) {
                            next += 1;
                        }
                        view.hits
                            .get(next)
                            .filter(|hit| hit.start <= place)
                            .map(|_| next)
                    });
                    let chosen = place
                        .zip(selected)
                        .is_some_and(|(place, (start, end))| start <= place && place < end);
                    // The current match is on the selection's ground while
                    // it is the selection, as find leaves it.
                    let ground = if focused || chosen {
                        SELECTED
                    } else if hit.is_some() {
                        INACTIVE_SELECTION
                    } else if run.style.code {
                        CHROME
                    } else {
                        PAPER
                    };
                    let ink = if ground == SELECTED {
                        PAPER
                    } else if link.is_some() {
                        ACCENT
                    } else {
                        INK
                    };
                    let extent = Rect {
                        x,
                        y,
                        width: right - glyph.x,
                        height: height as u32,
                    };
                    match pass {
                        Pass::Ground => {
                            if ground != PAPER {
                                fill(extent, ground, rect, sink);
                            }
                        }
                        Pass::Glyph => sink(Draw {
                            clip: rect,
                            primitive: Primitive::Glyph {
                                x,
                                y,
                                scalar: glyph.scalar,
                                style: GlyphStyle {
                                    ink,
                                    background: ground,
                                    ..piece.style
                                },
                            },
                        }),
                        Pass::Stroke => {
                            let stroke = |at: i64| Rect {
                                y: y + at,
                                height: s as u32,
                                ..extent
                            };
                            if run.style.strike {
                                fill(stroke(strike), ink, rect, sink);
                            }
                            if link.is_some() && (hovered || ground != SELECTED) {
                                fill(stroke(height - s), ink, rect, sink);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::document::tests::{cell, laid, Laid, STRIDE};
    use crate::document::{Link, Logical, Page, Run, Style, Wrapped};
    use td_ui::raster::TextFamily;
    use td_ui::{CELL_HEIGHT, CELL_WIDTH};

    const CW: i64 = CELL_WIDTH as i64;
    const CH: i64 = CELL_HEIGHT as i64;

    type Line<'a> = &'a [(&'a str, Option<usize>)];

    fn page(lines: &[Line], style: Style) -> Page {
        let mut page = Page::default();
        for (number, line) in lines.iter().enumerate() {
            let mut runs = Vec::new();
            for &(text, link) in line.iter() {
                if let Some(at) = link {
                    if page.links.len() == at {
                        page.links.push(Link {
                            written: format!("/{at}"),
                            line: number,
                        });
                    }
                }
                runs.push(Run {
                    text: text.into(),
                    style,
                    link,
                });
            }
            page.lines.push(Logical { runs, wrap: true });
        }
        page
    }

    /// `lines` to be laid on the cell, 100 cells wide.
    fn rendered(lines: &[Line]) -> Rendered {
        let page = crate::document::PageRef::for_test(page(lines, Style::default()));
        Rendered::new(&page, 800, TextFamily::Sans).unwrap()
    }

    /// `rendered` shown from its first line on `rows` lines.
    fn shown(mut rendered: Rendered, rows: usize) -> (Rendered, View) {
        let mut view = View::default();
        view.settle(&mut rendered, &mut cell(), rows);
        (rendered, view)
    }

    /// A pane of `rows` lines on the cell, a cell's inset left.
    fn pane(rows: i64) -> Pane {
        let metrics = cell();
        Pane {
            rect: Rect {
                x: 0,
                y: 0,
                width: 800,
                height: (rows * CH) as u32,
            },
            inset: CW,
            line: metrics.line(),
            scale: 1,
        }
    }

    fn at(logical: usize, run: usize, at: usize) -> Place {
        Place { logical, run, at }
    }

    #[test]
    fn scrolling_stays_inside_the_page() {
        let line: Line = &[("a", None)];
        let mut page = rendered(&[line; 10]);
        let metrics = &mut cell();
        let mut view = View::default();
        view.scroll(-3, &mut page, metrics, 4);
        assert_eq!(view.top, Place::line(0));
        view.scroll(100, &mut page, metrics, 4);
        assert_eq!(view.top, Place::line(6));
        view.home(&mut page, metrics, 20);
        view.end(&mut page, metrics, 20);
        assert_eq!(view.top, Place::line(0), "a page shorter than the screen");
    }

    /// One logical line of hundreds of wrapped lines scrolls a line at a
    /// time down to its end and back up to its start, each line where a
    /// wrap of all of it has it, holding no more than a few screenfuls.
    #[test]
    fn a_long_logical_line_scrolls_a_line_at_a_time_both_ways() {
        let words: Vec<String> = (0..1000).map(|n| format!("w{n}")).collect();
        let text = words.join(" ");
        let page = crate::document::PageRef::for_test(page(&[&[(&text, None)]], Style::default()));
        let width = 10 * CELL_WIDTH as u32;
        let all = Laid::of(
            Rendered::new(&page, width, TextFamily::Sans).unwrap(),
            &mut cell(),
        )
        .lines;
        assert!(all.len() > 4 * STRIDE, "{}", all.len());
        let (rows, metrics) = (7, &mut cell());
        let (mut rendered, mut view) =
            shown(Rendered::new(&page, width, TextFamily::Sans).unwrap(), rows);
        let last = all.len() - rows;
        for k in 1..=last + 3 {
            view.scroll(1, &mut rendered, metrics, rows);
            assert_eq!(view.top, all[k.min(last)].place(), "{k}");
            assert!(laid(&mut rendered).len() <= rows + 2 * STRIDE);
        }
        view.home(&mut rendered, metrics, rows);
        view.end(&mut rendered, metrics, rows);
        assert_eq!(view.top, all[last].place());
        for k in (0..last).rev() {
            view.scroll(-1, &mut rendered, metrics, rows);
            assert_eq!(view.top, all[k].place(), "{k}");
            assert!(laid(&mut rendered).len() <= rows + 2 * STRIDE);
            let shown: Vec<&Wrapped> = rendered.shown(view.top, rows).collect();
            assert_eq!(shown.len(), rows);
            assert!(shown.iter().zip(&all[k..]).all(|(a, b)| a.line == b.line));
        }
    }

    #[test]
    fn tab_starts_from_the_screen_and_goes_round() {
        let mut page = rendered(&[
            &[("x", Some(0))],
            &[("a", None)],
            &[("a", None)],
            &[("y", Some(1))],
            &[("z", Some(2))],
        ]);
        let metrics = &mut cell();
        let mut view = View {
            top: Place::line(2),
            ..View::default()
        };
        view.step_link(true, &mut page, metrics, 2);
        assert_eq!(view.focus, Some(1));
        view.step_link(true, &mut page, metrics, 2);
        view.step_link(true, &mut page, metrics, 2);
        assert_eq!(view.focus, Some(0));
        assert_eq!(view.top, Place::line(0), "revealed");
        let mut view = View::default();
        view.step_link(false, &mut page, metrics, 2);
        assert_eq!(view.focus, Some(0), "the last link above the bottom");
        view.step_link(false, &mut page, metrics, 2);
        assert_eq!(view.focus, Some(2), "round");
        assert_eq!(view.top, Place::line(3), "revealed at the bottom");
    }

    #[test]
    fn find_ignores_case_and_steps_round() {
        let mut page = rendered(&[
            &[("Alpha ", None), ("beta", Some(0))],
            &[("ALPHA alpha", None)],
        ]);
        let metrics = &mut cell();
        let mut view = View::default();
        view.find("alpha", &mut page, metrics, 5);
        assert_eq!(
            view.hits,
            [
                Hit {
                    start: at(0, 0, 0),
                    end: at(0, 0, 5)
                },
                Hit {
                    start: at(1, 0, 0),
                    end: at(1, 0, 5)
                },
                Hit {
                    start: at(1, 0, 6),
                    end: at(1, 0, 11)
                },
            ]
        );
        view.step_hit(false, &mut page, metrics, 5);
        assert_eq!(view.at, 2);
        view.find("a b", &mut page, metrics, 5);
        assert_eq!(
            view.hits,
            [Hit {
                start: at(0, 0, 4),
                end: at(0, 1, 1)
            }],
            "across runs"
        );
        view.find("", &mut page, metrics, 5);
        assert!(view.hits.is_empty());
    }

    /// Find searches a logical line's text, so a match runs on across the
    /// end of one of its wrapped lines, and each line marks its part.
    #[test]
    fn a_match_across_a_wrapped_lines_end_is_marked_on_both() {
        let page = crate::document::PageRef::for_test(page(
            &[&[("one two three", None)]],
            Style::default(),
        ));
        let mut rendered = Rendered::new(&page, 5 * CELL_WIDTH as u32, TextFamily::Sans).unwrap();
        let metrics = &mut cell();
        let mut view = View::default();
        view.find("ONE TWO", &mut rendered, metrics, 3);
        assert_eq!(view.hits.len(), 1);
        let (_, glyphs) = painted(&rendered, &view, None, &pane(3));
        let selected: String = glyphs
            .iter()
            .filter(|glyph| glyph.4 == SELECTED)
            .map(|glyph| glyph.2)
            .collect();
        assert_eq!(selected, "onetwo");
        assert_eq!(glyphs.len(), "onetwothree".len());
    }

    /// A match starting with the space a wrapped line drops at its end is
    /// shown on the line its first drawn glyph is on, and one running onto
    /// the next line shows that line too.
    #[test]
    fn a_match_from_a_dropped_space_is_shown_where_it_is_drawn() {
        let text = "word ".repeat(200);
        let page = crate::document::PageRef::for_test(page(&[&[(&text, None)]], Style::default()));
        // Nine cells: "word word" a line, the space after it dropped.
        let mut rendered = Rendered::new(&page, 9 * CELL_WIDTH as u32, TextFamily::Sans).unwrap();
        let (metrics, rows) = (&mut cell(), 3);
        let mut view = View::default();
        view.find(" w", &mut rendered, metrics, rows);
        assert_eq!(view.hits.len(), 199);
        for _ in 0..view.hits.len() {
            let (_, glyphs) = painted(&rendered, &view, None, &pane(rows as i64));
            let marked: String = glyphs
                .iter()
                .filter(|glyph| glyph.4 == SELECTED)
                .map(|glyph| glyph.2)
                .collect();
            assert!(marked == " w" || marked == "w", "{:?}: {marked:?}", view.at);
            view.step_hit(true, &mut rendered, metrics, rows);
        }
        // On one line a match across two shows its start.
        view.find("d w", &mut rendered, metrics, rows);
        view.step_hit(true, &mut rendered, metrics, 1);
        let (_, glyphs) = painted(&rendered, &view, None, &pane(1));
        assert!(glyphs.iter().any(|glyph| glyph.4 == SELECTED));
    }

    /// Find goes through the text once, overlapping starts included, and
    /// its matches do not overlap.
    #[test]
    fn find_takes_the_first_of_overlapping_matches() {
        let mut page = rendered(&[&[("aaab aaaa", None)]]);
        let (metrics, mut view) = (&mut cell(), View::default());
        view.find("aab", &mut page, metrics, 1);
        assert_eq!(
            view.hits,
            [Hit {
                start: at(0, 0, 1),
                end: at(0, 0, 4)
            }]
        );
        view.find("aa", &mut page, metrics, 1);
        let starts: Vec<usize> = view.hits.iter().map(|hit| hit.start.at).collect();
        assert_eq!(starts, [0, 5, 7]);
        view.find("abab", &mut page, metrics, 1);
        assert!(view.hits.is_empty());
    }

    /// A tab is found as it is shown, a space, in the query as on the
    /// page.
    #[test]
    fn find_takes_a_tab_as_a_space() {
        let mut page = rendered(&[&[("a\tb c", None)]]);
        let (metrics, mut view) = (&mut cell(), View::default());
        for (query, start) in [("a\tb", 0), ("a b", 0), ("b\tc", 2)] {
            view.find(query, &mut page, metrics, 1);
            let starts: Vec<usize> = view.hits.iter().map(|hit| hit.start.at).collect();
            assert_eq!(starts, [start], "{query:?}");
        }
    }

    /// An unwrapped line is laid only to the pane's edge and `OVERHANG`
    /// past it, and find matches only what is laid: a match past the edge
    /// at a narrow width, found at a wide one.
    #[test]
    fn find_matches_an_unwrapped_line_only_as_far_as_it_is_laid() {
        let text = format!("{}needle", "x".repeat(200));
        let mut page = page(&[&[(&text, None)]], Style::default());
        page.lines[0].wrap = false;
        let page = crate::document::PageRef::for_test(page);
        let metrics = &mut cell();
        let mut view = View::default();
        let mut narrow = Rendered::new(&page, 800, TextFamily::Sans).unwrap();
        view.find("needle", &mut narrow, metrics, 1);
        assert!(view.hits.is_empty());
        let mut wide = Rendered::new(&page, 300 * CELL_WIDTH as u32, TextFamily::Sans).unwrap();
        view.find("needle", &mut wide, metrics, 1);
        assert_eq!(
            view.hits,
            [Hit {
                start: at(0, 0, 200),
                end: at(0, 0, 206)
            }]
        );
        view.remark("needle", &narrow, metrics);
        assert!(view.hits.is_empty(), "made again at the narrow width");
    }

    #[test]
    fn a_pixel_names_the_link_it_shows() {
        let (page, view) = shown(
            rendered(&[&[("go ", None), ("here", Some(0))], &[("x", None)]]),
            2,
        );
        let at = |column: i64| CW + column * CW + 1;
        assert_eq!(view.link_at(&page, &pane(2), at(3), 1), Some(0));
        assert_eq!(view.link_at(&page, &pane(2), at(6), 1), Some(0));
        assert_eq!(view.link_at(&page, &pane(2), at(7), 1), None);
        assert_eq!(view.link_at(&page, &pane(2), at(1), 1), None);
        assert_eq!(view.link_at(&page, &pane(2), at(0), 5 * CH), None);
        let (page, view) = shown(rendered(&[&[("here", Some(0))], &[("here", Some(0))]]), 2);
        assert_eq!(view.link_at(&page, &pane(2), at(0), CH + 1), Some(0));
        assert_eq!(view.link_at(&page, &pane(2), CW - 1, 1), None, "the inset");
        let mut short = pane(2);
        short.rect.height += (CH - 1) as u32;
        assert_eq!(
            view.link_at(&page, &short, at(0), 2 * CH + 1),
            None,
            "a row not shown whole"
        );
    }

    /// The draws `paint` streams for `rendered`, as fills (rect, colour)
    /// and glyphs (x, y, scalar, ink, ground).
    #[allow(clippy::type_complexity)]
    fn painted(
        rendered: &Rendered,
        view: &View,
        hover: Option<usize>,
        pane: &Pane,
    ) -> (Vec<(Rect, u32)>, Vec<(i64, i64, char, u32, u32)>) {
        let (mut fills, mut glyphs) = (Vec::new(), Vec::new());
        paint(
            rendered,
            view,
            hover,
            pane,
            &mut |draw| match draw.primitive {
                Primitive::Fill { rect, color } => fills.push((rect, color)),
                Primitive::Glyph {
                    x,
                    y,
                    scalar,
                    style,
                } => glyphs.push((x, y, scalar, style.ink, style.background)),
                _ => {}
            },
        );
        (fills, glyphs)
    }

    #[test]
    fn a_line_paints_its_glyphs_at_their_pens_with_their_looks() {
        let mut page = rendered(&[&[("ab ", None), ("cd", Some(0))], &[("ab", None)]]);
        let mut view = View::default();
        view.find("b", &mut page, &mut cell(), 2);
        let (fills, glyphs) = painted(&page, &view, None, &pane(2));
        let pens: Vec<(i64, i64, char)> = glyphs.iter().map(|g| (g.0, g.1, g.2)).collect();
        assert_eq!(
            pens,
            [
                (CW, 0, 'a'),
                (2 * CW, 0, 'b'),
                (3 * CW, 0, ' '),
                (4 * CW, 0, 'c'),
                (5 * CW, 0, 'd'),
                (CW, CH, 'a'),
                (2 * CW, CH, 'b'),
            ]
        );
        // The current match on SELECTED, the other on its own ground.
        assert_eq!(glyphs[1].3, PAPER);
        assert_eq!(glyphs[1].4, SELECTED);
        assert_eq!(glyphs[6].4, INACTIVE_SELECTION);
        assert_eq!(glyphs[3].3, ACCENT, "a link's ink");
        // A link is underlined, a cell under each glyph.
        let underlines: Vec<Rect> = fills
            .iter()
            .filter(|(rect, color)| *color == ACCENT && rect.height == 1)
            .map(|(rect, _)| *rect)
            .collect();
        assert_eq!(underlines.len(), 2);
        assert_eq!(underlines[0].y, CH - 1);
        assert_eq!(underlines[0].x, 4 * CW);
        // The focused link is SELECTED and not underlined.
        view.hits.clear();
        view.focus = Some(0);
        let (fills, glyphs) = painted(&page, &view, None, &pane(2));
        assert_eq!(glyphs[3].4, SELECTED);
        assert!(fills
            .iter()
            .all(|(rect, color)| !(rect.height == 1 && rect.y == CH - 1 && *color == PAPER)));
        assert!(fills.iter().any(|(rect, color)| *color == SELECTED
            && *rect
                == Rect {
                    x: 4 * CW,
                    y: 0,
                    width: CW as u32,
                    height: CH as u32
                }));
    }

    #[test]
    fn struck_and_code_text_paint_their_own_stroke_and_ground() {
        let struck = Style {
            strike: true,
            ..Style::default()
        };
        let struck = crate::document::PageRef::for_test(page(&[&[("ab", None)]], struck));
        let (rendered, view) = shown(Rendered::new(&struck, 800, TextFamily::Sans).unwrap(), 1);
        let (fills, _) = painted(&rendered, &view, None, &pane(1));
        let strokes: Vec<Rect> = fills
            .iter()
            .filter(|(_, color)| *color == INK)
            .map(|(rect, _)| *rect)
            .collect();
        assert_eq!(strokes.len(), 2);
        assert_eq!((strokes[0].x, strokes[0].y, strokes[0].height), (CW, 9, 1));
        let code = Style {
            code: true,
            ..Style::default()
        };
        let code = crate::document::PageRef::for_test(page(&[&[("ab", None)]], code));
        let (rendered, view) = shown(Rendered::new(&code, 800, TextFamily::Sans).unwrap(), 1);
        let (fills, glyphs) = painted(&rendered, &view, None, &pane(1));
        assert_eq!(
            fills.iter().filter(|(_, color)| *color == CHROME).count(),
            2
        );
        assert!(glyphs.iter().all(|glyph| glyph.4 == CHROME));
    }

    #[test]
    fn a_line_wider_than_the_pane_is_cut_at_its_edge() {
        let wide = "x".repeat(1000);
        let mut page = page(&[&[(&wide, None)]], Style::default());
        page.lines[0].wrap = false;
        let (rendered, view) = shown(
            Rendered::new(
                &crate::document::PageRef::for_test(page),
                800,
                TextFamily::Sans,
            )
            .unwrap(),
            1,
        );
        assert_eq!(rendered.shown(view.top, 5).count(), 1);
        let (_, glyphs) = painted(&rendered, &view, None, &pane(1));
        let last = glyphs.last().unwrap().0;
        assert!(last < 800 + i64::from(OVERHANG), "{last}");
        assert!(last >= 800, "{last}");
    }

    #[test]
    fn a_pile_of_marks_at_one_pen_draws_only_its_first() {
        let marks = "\u{301}".repeat(100);
        let mut page = page(&[&[(&marks, None)]], Style::default());
        page.lines[0].wrap = false;
        let (mut rendered, view) = shown(
            Rendered::new(
                &crate::document::PageRef::for_test(page),
                800,
                TextFamily::Sans,
            )
            .unwrap(),
            1,
        );
        // As Noto measures them: no advance.
        let line = &mut laid(&mut rendered)[0].line;
        for glyph in &mut line.glyphs {
            glyph.x = 0;
        }
        line.pieces[0].right = 0;
        line.width = 0;
        let (_, glyphs) = painted(&rendered, &view, None, &pane(1));
        assert_eq!(glyphs.len(), MAX_STACKED);
    }

    /// `lines` wrapped `columns` cells wide and shown from the first on
    /// `rows` lines.
    fn narrow(lines: &[Line], columns: u32, rows: usize) -> (Rendered, View) {
        let page = crate::document::PageRef::for_test(page(lines, Style::default()));
        shown(
            Rendered::new(&page, columns * CELL_WIDTH as u32, TextFamily::Sans).unwrap(),
            rows,
        )
    }

    /// A point falls between the characters by the halves of the glyphs
    /// it is among, on the character under it; left of a line's text at
    /// its start, right of it or in the right inset at its end, above the
    /// pane at the first line's start and below the lines at the last's
    /// end; over a continuation's repeated marks at its own text's start;
    /// and right of a line that is not wrapped at its logical line's end.
    #[test]
    fn a_point_falls_between_characters_by_their_halves() {
        // "one" / "two" / "three", a cell a glyph.
        let (rendered, view) = narrow(&[&[("one two three", None)]], 5, 3);
        let three = pane(3);
        let spot = |x: i64, y: i64| view.spot(&rendered, &three, x, y).unwrap();
        let caret = |x: i64, y: i64| spot(x, y).caret;
        assert_eq!(
            spot(CW + 3, 1),
            Spot {
                caret: at(0, 0, 0),
                under: Some(at(0, 0, 0))
            }
        );
        assert_eq!(
            spot(CW + 5, 1),
            Spot {
                caret: at(0, 0, 1),
                under: Some(at(0, 0, 0))
            },
            "the right half of 'o'"
        );
        assert_eq!(
            spot(CW - 1, 1),
            Spot {
                caret: at(0, 0, 0),
                under: None
            },
            "the inset"
        );
        assert_eq!(caret(CW + 3 * CW + 2, 1), at(0, 0, 3), "past 'one'");
        assert_eq!(spot(CW + 3 * CW + 2, 1).under, None);
        assert_eq!(caret(800 - CW, 1), at(0, 0, 3), "the right inset");
        assert_eq!(caret(CW + 1, CH + 1), at(0, 0, 4), "'two'");
        assert_eq!(caret(CW + 5, -40), at(0, 0, 0), "above the pane");
        assert_eq!(caret(CW + 5, 3 * CH + 1), at(0, 0, 13), "below the lines");
        // The repeated mark of "> beta" is no page text.
        let (rendered, view) = narrow(&[&[("> ", None), ("alpha beta gamma", None)]], 10, 3);
        let lines: Vec<String> = rendered
            .shown(view.top, 3)
            .map(|w| w.line.glyphs.iter().map(|g| g.scalar).collect())
            .collect();
        assert_eq!(lines, ["> alpha", ">beta", ">gamma"]);
        let spot = view.spot(&rendered, &three, CW + 1, CH + 1).unwrap();
        assert_eq!(
            spot,
            Spot {
                caret: at(0, 1, 6),
                under: None
            }
        );
        // A line that is not wrapped ends where its logical line does.
        let wide = "x".repeat(200);
        let mut unwrapped = page(&[&[(&wide, None)]], Style::default());
        unwrapped.lines[0].wrap = false;
        let (rendered, view) = shown(
            Rendered::new(
                &crate::document::PageRef::for_test(unwrapped),
                800,
                TextFamily::Sans,
            )
            .unwrap(),
            1,
        );
        let one = pane(1);
        let laid = rendered
            .shown(view.top, 1)
            .next()
            .unwrap()
            .line
            .glyphs
            .len();
        assert!(laid < 200);
        assert_eq!(
            view.spot(&rendered, &one, 800 - 1, 1).unwrap().caret,
            at(0, 0, 200)
        );
        assert_eq!(
            view.spot(&rendered, &one, CW + 10 * CW, 1).unwrap().caret,
            at(0, 0, 10)
        );
        let (empty, view) = shown(rendered_empty(), 1);
        assert_eq!(view.spot(&empty, &one, CW, 1), None);
    }

    fn rendered_empty() -> Rendered {
        Rendered::new(
            &crate::document::PageRef::for_test(Page::default()),
            800,
            TextFamily::Sans,
        )
        .unwrap()
    }

    /// The glyphs painted on the selection's ground.
    fn chosen(rendered: &Rendered, view: &View, rows: i64) -> (String, String) {
        let (_, glyphs) = painted(rendered, view, None, &pane(rows));
        let on = glyphs.iter().filter(|glyph| glyph.4 == SELECTED);
        let off = glyphs.iter().filter(|glyph| glyph.4 != SELECTED);
        (
            on.map(|glyph| glyph.2).collect(),
            off.map(|glyph| glyph.2).collect(),
        )
    }

    /// The selection is painted glyph by glyph where it is laid, across
    /// the ends of wrapped lines, never on a continuation's repeated
    /// marks, and on a line not wrapped only as far as it is laid.
    #[test]
    fn a_selection_paints_only_laid_glyphs_on_its_ground() {
        let (rendered, mut view) = narrow(&[&[("one two three", None)]], 5, 3);
        view.select(Some(Selection {
            anchor: at(0, 0, 10),
            focus: at(0, 0, 2),
        }));
        assert_eq!(chosen(&rendered, &view, 3).0, "etwoth");
        // Empty, it paints nothing.
        view.select(Some(Selection {
            anchor: at(0, 0, 4),
            focus: at(0, 0, 4),
        }));
        assert_eq!(chosen(&rendered, &view, 3).0, "");
        let (rendered, mut view) = narrow(&[&[("> ", None), ("alpha beta gamma", None)]], 10, 3);
        view.select(Some(Selection {
            anchor: at(0, 0, 0),
            focus: at(0, 1, 16),
        }));
        assert_eq!(
            chosen(&rendered, &view, 3),
            ("> alphabetagamma".to_string(), ">>".to_string())
        );
        let wide = "x".repeat(200);
        let mut unwrapped = page(&[&[(&wide, None)], &[("y", None)]], Style::default());
        unwrapped.lines[0].wrap = false;
        let (rendered, mut view) = shown(
            Rendered::new(
                &crate::document::PageRef::for_test(unwrapped),
                800,
                TextFamily::Sans,
            )
            .unwrap(),
            2,
        );
        view.select(Some(Selection {
            anchor: at(0, 0, 0),
            focus: at(1, 0, 1),
        }));
        let (on, off) = chosen(&rendered, &view, 2);
        assert!(off.is_empty());
        let laid = rendered
            .shown(view.top, 1)
            .next()
            .unwrap()
            .line
            .glyphs
            .len();
        assert!(
            on.len() > 100 && on.len() <= laid + 1,
            "{} of {laid}",
            on.len()
        );
        // A selection takes the focused link's place, and focusing a link
        // the selection's.
        let (mut rendered, mut view) = narrow(&[&[("go", Some(0))], &[("x", None)]], 10, 2);
        view.step_link(true, &mut rendered, &mut cell(), 2);
        assert_eq!(view.focus, Some(0));
        view.select(Some(Selection {
            anchor: at(1, 0, 0),
            focus: at(1, 0, 1),
        }));
        assert_eq!(view.focus, None);
        view.step_link(true, &mut rendered, &mut cell(), 2);
        assert_eq!((view.focus, view.selection), (Some(0), None));
    }

    /// A combining mark of no advance is never parted from its base: no
    /// place falls between them, though the pointer is past the base's
    /// middle and before the mark's pen.
    #[test]
    fn no_place_falls_between_a_base_and_its_mark() {
        let (mut rendered, view) = narrow(&[&[("e\u{301}x", None)]], 10, 1);
        // As Noto measures them: the mark takes no advance.
        let line = &mut laid(&mut rendered)[0].line;
        assert_eq!(line.glyphs.len(), 3);
        line.glyphs[1].x = CW as u32;
        line.glyphs[2].x = CW as u32;
        line.width = 2 * CW as u32;
        let one = pane(1);
        let spot = |x: i64| view.spot(&rendered, &one, CW + x, 1).unwrap();
        assert_eq!(spot(2).caret, at(0, 0, 0));
        assert_eq!(
            spot(6),
            Spot {
                caret: at(0, 0, 3),
                under: Some(at(0, 0, 0))
            },
            "past the base's middle: after its mark"
        );
        assert_eq!(spot(CW + 2).caret, at(0, 0, 3));
        assert_eq!(spot(CW + 6).caret, at(0, 0, 4));
    }

    /// A line that begins with a mark of no advance still has a start a
    /// point reaches: left of the mark, in the inset, or at its pen; and
    /// a line of such marks alone has its start there and its end
    /// right of them.
    #[test]
    fn a_line_beginning_with_a_mark_of_no_advance_has_a_start() {
        let (mut rendered, view) = narrow(&[&[("\u{301}ab", None)]], 10, 1);
        let line = &mut laid(&mut rendered)[0].line;
        assert_eq!(line.glyphs.len(), 3);
        line.glyphs[1].x = 0;
        line.glyphs[2].x = CW as u32;
        line.width = 2 * CW as u32;
        let one = pane(1);
        let caret =
            |rendered: &Rendered, x: i64| view.spot(rendered, &one, CW + x, 1).unwrap().caret;
        assert_eq!(caret(&rendered, -1), at(0, 0, 0), "the inset");
        assert_eq!(caret(&rendered, 0), at(0, 0, 0), "the line's left edge");
        assert_eq!(caret(&rendered, 2), at(0, 0, 2), "before 'a'");
        assert_eq!(caret(&rendered, 6), at(0, 0, 3), "after 'a'");
        let (mut rendered, view) = narrow(&[&[("\u{301}\u{301}", None)]], 10, 1);
        let line = &mut laid(&mut rendered)[0].line;
        for glyph in &mut line.glyphs {
            glyph.x = 0;
        }
        line.width = 0;
        let caret = |x: i64| view.spot(&rendered, &one, CW + x, 1).unwrap().caret;
        assert_eq!(caret(-1), at(0, 0, 0));
        assert_eq!(caret(0), at(0, 0, 0));
        assert_eq!(caret(5), at(0, 0, 4), "right of them, the end");
    }

    /// Tab clears the selection on a page with no link to focus too.
    #[test]
    fn tab_clears_the_selection_without_links() {
        let (mut rendered, mut view) = narrow(&[&[("no links", None)]], 80, 1);
        view.select(Some(Selection {
            anchor: at(0, 0, 0),
            focus: at(0, 0, 2),
        }));
        view.step_link(true, &mut rendered, &mut cell(), 1);
        assert_eq!((view.focus, view.selection), (None, None));
    }

    /// The current match is on the selection's ground only while it is
    /// the selection: once another is made, it is a match like the rest.
    #[test]
    fn the_current_match_is_painted_selected_only_while_it_is_the_selection() {
        let (mut rendered, mut view) = narrow(&[&[("ab ab", None)]], 80, 1);
        view.find("ab", &mut rendered, &mut cell(), 1);
        let grounds = |view: &View| -> Vec<u32> {
            let (_, glyphs) = painted(&rendered, view, None, &pane(1));
            glyphs.iter().map(|glyph| glyph.4).collect()
        };
        let (on, off, plain) = (SELECTED, INACTIVE_SELECTION, PAPER);
        assert_eq!(grounds(&view), [on, on, plain, off, off]);
        view.select(Some(Selection {
            anchor: at(0, 0, 3),
            focus: at(0, 0, 4),
        }));
        assert_eq!(grounds(&view), [off, off, plain, on, off]);
    }

    /// Find's current match is the selection, and a query that matches
    /// nothing leaves none.
    #[test]
    fn finds_current_match_is_selected() {
        let (mut rendered, mut view) = narrow(&[&[("alpha beta alpha", None)]], 80, 1);
        let metrics = &mut cell();
        view.find("alpha", &mut rendered, metrics, 1);
        let selected = |view: &View| view.selection.and_then(Selection::range);
        assert_eq!(selected(&view), Some((at(0, 0, 0), at(0, 0, 5))));
        view.step_hit(true, &mut rendered, metrics, 1);
        assert_eq!(selected(&view), Some((at(0, 0, 11), at(0, 0, 16))));
        view.remark("alpha", &rendered, metrics);
        assert_eq!(selected(&view), Some((at(0, 0, 11), at(0, 0, 16))), "kept");
        view.find("zeta", &mut rendered, metrics, 1);
        assert_eq!(view.selection, None);
    }

    /// Find and the selection work on a page's decoded text: a query
    /// matches a page sent in windows-1252, ignoring case, and its
    /// selection copies the characters, not the bytes sent.
    #[test]
    fn find_and_selection_see_a_legacy_page_as_its_text() {
        let url = td_url::Url::parse("https://example.org/").unwrap();
        let doc = crate::document::Document::from_reply(
            url,
            200,
            Some("text/html; charset=windows-1252"),
            b"<p>Caf\xe9 cr\xe8me br\xfbl\xe9e, \x80 5</p>".to_vec(),
        );
        let (mut rendered, mut view) = shown(
            doc.render(80 * CELL_WIDTH as u32, TextFamily::Sans)
                .unwrap(),
            1,
        );
        let metrics = &mut cell();
        view.find("CRÈME BRÛLÉE", &mut rendered, metrics, 1);
        let (start, end) = view.selection.and_then(Selection::range).unwrap();
        assert_eq!(
            rendered.page.text(start, end, 100).as_deref(),
            Some("crème brûlée")
        );
        view.find("€", &mut rendered, metrics, 1);
        assert_eq!(view.hits.len(), 1);
        let all = rendered.page.line_end(0);
        assert_eq!(
            rendered.page.text(Place::default(), all, 100).as_deref(),
            Some("Café crème brûlée, € 5")
        );
    }
}
