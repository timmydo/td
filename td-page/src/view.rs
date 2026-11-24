//! What the window shows of a laid page (td-web/DESIGN.md §10, "Places, find,
//! selection and links" and "Scrolling"): where it is scrolled to in
//! device pixels, the focused link, find's matches and the selection,
//! and the top's place kept through a relayout.

use crate::fonts::Measure;
use crate::page::{meeting, Laid, CHUNK};
use crate::paint::Frame;
use crate::search::Hit;
use crate::text::{self, Item, Piece, Place};
use td_alloc::BudgetVec;
use td_html::document::NodeId;
use td_ui::raster::Rect;

/// The page's pane in the window: its rectangle, the inset each side and
/// the scale.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pane {
    pub rect: Rect,
    pub inset: i64,
    pub scale: i64,
    /// A body line, in device pixels: what `Up` and `Down` scroll.
    pub line: i64,
}

impl Pane {
    pub fn height(&self) -> i64 {
        i64::from(self.rect.height)
    }
    pub fn width(&self) -> i64 {
        i64::from(self.rect.width) - 2 * self.inset
    }
}

/// Two places, the first where a selection began.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub anchor: Place,
    pub focus: Place,
}

impl Selection {
    /// Its places in order; none when empty.
    pub fn range(self) -> Option<(Place, Place)> {
        let (start, end) = if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        };
        (start != end).then_some((start, end))
    }
}

/// What the reader sees, kept through a relayout: the place the first
/// line reaching below the pane's top begins, and the CSS px from that
/// line's top to the pane's.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Anchor {
    pub place: Place,
    pub dy: f64,
}

#[derive(Default)]
pub struct View {
    /// Scrolled down and across, in device pixels.
    pub y: i64,
    pub x: i64,
    /// The focused link: its index among the laid links.
    pub focus: Option<usize>,
    pub hits: Option<BudgetVec<Hit>>,
    /// The match last shown.
    pub at: usize,
    pub selection: Option<Selection>,
}

/// The page's extent in device pixels: its overflow's right and bottom.
fn extent(laid: &Laid, scale: i64) -> (i64, i64) {
    let o = laid.layout.overflow;
    let s = scale as f64;
    (
        ((o.x + o.width).max(0.0) * s).ceil() as i64,
        ((o.y + o.height).max(laid.layout.height).max(0.0) * s).ceil() as i64,
    )
}

impl View {
    pub fn hits(&self) -> &[Hit] {
        self.hits.as_ref().map_or(&[], |hits| hits.as_slice())
    }

    /// The frame painting from this view puts the page in.
    pub fn frame(&self, pane: &Pane) -> Frame {
        Frame {
            pane: pane.rect,
            x: pane.rect.x + pane.inset - self.x,
            y: pane.rect.y - self.y,
            scale: pane.scale,
        }
    }

    /// Scrolled into the page's extent.
    pub fn clamp(&mut self, laid: &Laid, pane: &Pane) {
        let (right, bottom) = extent(laid, pane.scale);
        self.y = self.y.min(bottom - pane.height()).max(0);
        self.x = self.x.min(right - pane.width()).max(0);
    }

    pub fn scroll(&mut self, laid: &Laid, pane: &Pane, dy: i64, dx: i64) {
        self.y = self.y.saturating_add(dy);
        self.x = self.x.saturating_add(dx);
        self.clamp(laid, pane);
    }

    pub fn home(&mut self) {
        self.y = 0;
        self.x = 0;
    }

    pub fn end(&mut self, laid: &Laid, pane: &Pane) {
        self.y = i64::MAX / 4;
        self.clamp(laid, pane);
    }

    /// The pane's top and bottom in the page's CSS px.
    fn shown(&self, pane: &Pane) -> (f64, f64) {
        let s = pane.scale.max(1) as f64;
        (self.y as f64 / s, (self.y + pane.height()) as f64 / s)
    }

    /// Scrolls so line `at`, and `x` CSS px along it, are in the pane.
    pub fn show_line(&mut self, laid: &Laid, pane: &Pane, at: usize, x: Option<f64>) {
        let Some(line) = laid.lines().get(at) else {
            return;
        };
        let s = pane.scale.max(1) as f64;
        let top = (line.y * s).floor() as i64;
        let bottom = ((line.y + line.height) * s).ceil() as i64;
        if top < self.y {
            self.y = top;
        } else if bottom > self.y + pane.height() {
            self.y = bottom - pane.height();
        }
        if let Some(x) = x {
            let x = (x * s).round() as i64;
            let margin = pane.line;
            if x < self.x {
                self.x = (x - margin).max(0);
            } else if x > self.x + pane.width() {
                self.x = x - pane.width() + margin;
            }
        }
        self.clamp(laid, pane);
    }

    /// Scrolls `place` into the pane.
    pub fn reveal(&mut self, laid: &Laid, measure: &mut Measure, pane: &Pane, place: Place) {
        if let Some((at, x)) = text::locate(laid, measure, place) {
            self.show_line(laid, pane, at, Some(x));
        }
    }

    /// The lines meeting the pane, by index.
    fn lines_shown<'a>(&self, laid: &'a Laid, pane: &Pane) -> impl Iterator<Item = usize> + 'a {
        let (top, bottom) = self.shown(pane);
        let count = laid.lines().len();
        meeting(laid.line_spans(), top, bottom)
            .flat_map(move |chunk| (chunk * CHUNK)..((chunk + 1) * CHUNK).min(count))
            .filter(move |&at| {
                laid.lines()
                    .get(at)
                    .is_some_and(|l| l.y < bottom && l.y + l.height > top)
            })
    }

    /// The top's place: the first line reaching below the pane's top.
    pub fn anchor(&self, laid: &Laid, pane: &Pane) -> Option<Anchor> {
        let (top, _) = self.shown(pane);
        let lines = laid.lines();
        let at = self
            .lines_shown(laid, pane)
            .filter(|&at| {
                lines
                    .get(at)
                    .is_some_and(|l| text::bounds(laid, l).is_some())
            })
            .min_by(|&a, &b| {
                let ya = lines.get(a).map_or(0.0, |l| l.y);
                let yb = lines.get(b).map_or(0.0, |l| l.y);
                ya.total_cmp(&yb).then(a.cmp(&b))
            })?;
        let line = lines.get(at)?;
        let (place, _) = text::bounds(laid, line)?;
        Some(Anchor {
            place,
            dy: top - line.y,
        })
    }

    /// Shows `anchor`'s line at its offset again.
    pub fn restore(&mut self, laid: &Laid, pane: &Pane, anchor: Anchor) {
        let at = text::line_of(laid, anchor.place);
        if let Some(line) = laid.lines().get(at) {
            self.y = ((line.y + anchor.dy) * pane.scale as f64).round() as i64;
        }
        self.clamp(laid, pane);
    }

    /// The status row's place: the last line shown whole, counted from
    /// one, of the layout's lines.
    pub fn bottom(&self, laid: &Laid, pane: &Pane) -> (usize, usize) {
        let (top, bottom) = self.shown(pane);
        let whole = self
            .lines_shown(laid, pane)
            .filter(|&at| {
                laid.lines()
                    .get(at)
                    .is_some_and(|l| l.y >= top - 0.5 && l.y + l.height <= bottom + 0.5)
            })
            .max();
        let shown = whole.map_or(0, |at| at + 1);
        (shown, laid.lines().len())
    }

    /// A point of the window as the page's CSS px.
    pub fn point(&self, pane: &Pane, x: i64, y: i64) -> (f64, f64) {
        let frame = self.frame(pane);
        let s = pane.scale.max(1) as f64;
        ((x - frame.x) as f64 / s, (y - frame.y) as f64 / s)
    }

    /// The place nearest a point of the window, and the piece under it.
    pub fn spot(
        &self,
        laid: &Laid,
        measure: &mut Measure,
        pane: &Pane,
        x: i64,
        y: i64,
    ) -> Option<(Place, Option<Item>)> {
        let (x, y) = self.point(pane, x, y);
        text::place_at(laid, measure, x, y)
    }

    /// The link under a point of the window: its index among the laid
    /// links.
    pub fn link_at(
        &self,
        laid: &Laid,
        measure: &mut Measure,
        pane: &Pane,
        x: i64,
        y: i64,
    ) -> Option<usize> {
        if !pane.rect.contains(x, y) {
            return None;
        }
        let (_, item) = self.spot(laid, measure, pane, x, y)?;
        let item = item?;
        let line = laid.lines().get(text::line_of(laid, item.start))?;
        let node = match item.piece {
            Piece::Text(i) => line.fragments.get(i).map(|f| f.node),
            Piece::Replaced(i) => line.replaced.get(i).map(|r| r.node),
        }?;
        let link = laid.tree().link(node)?;
        link_index(laid, link)
    }

    /// Steps the focused link forward or back in tree order, showing it;
    /// with none focused, from the screen: the first link on a line
    /// shown or after, or the last on a line shown or before, else round.
    /// A link's first line is not in tree order past a float's lines, so
    /// the links are scanned.
    pub fn step_link(&mut self, laid: &Laid, measure: &mut Measure, pane: &Pane, forward: bool) {
        let links = laid.links();
        let count = links.len();
        if count == 0 {
            return;
        }
        let next = match (self.focus, forward) {
            (None, true) => self
                .lines_shown(laid, pane)
                .min()
                .and_then(|top| links.iter().position(|&(_, line)| line as usize >= top))
                .unwrap_or(0),
            (None, false) => self
                .lines_shown(laid, pane)
                .max()
                .and_then(|bottom| links.iter().rposition(|&(_, line)| line as usize <= bottom))
                .unwrap_or(count - 1),
            (Some(at), true) => (at + 1) % count,
            (Some(at), false) => (at + count - 1) % count,
        };
        self.focus = Some(next);
        if let Some(&(link, line)) = laid.links().get(next) {
            let place = first_place(laid, link, line as usize);
            match place {
                Some(place) => self.reveal(laid, measure, pane, place),
                None => self.show_line(laid, pane, line as usize, None),
            }
        }
    }

    /// Steps to the next or previous match, selecting it and showing it.
    pub fn step_hit(&mut self, laid: &Laid, measure: &mut Measure, pane: &Pane, forward: bool) {
        let count = self.hits().len();
        if count == 0 {
            return;
        }
        self.at = if forward {
            (self.at + 1) % count
        } else {
            (self.at + count - 1) % count
        };
        self.show_hit(laid, measure, pane);
    }

    /// Selects match `at` and shows it.
    pub fn show_hit(&mut self, laid: &Laid, measure: &mut Measure, pane: &Pane) {
        if let Some(hit) = self.hits().get(self.at).copied() {
            self.selection = Some(Selection {
                anchor: hit.start,
                focus: hit.end,
            });
            self.reveal(laid, measure, pane, hit.end);
            self.reveal(laid, measure, pane, hit.start);
        }
    }

    /// The first match at or after the top's place.
    pub fn first_hit_shown(&self, laid: &Laid, pane: &Pane) -> usize {
        let top = self
            .anchor(laid, pane)
            .map_or(Place::default(), |a| a.place);
        let at = self.hits().partition_point(|hit| hit.start < top);
        if at >= self.hits().len() {
            0
        } else {
            at
        }
    }
}

/// A link's index among the laid links.
pub fn link_index(laid: &Laid, link: NodeId) -> Option<usize> {
    let rank = laid.tree().rank(link);
    let links = laid.links();
    let at = links.partition_point(|&(l, _)| laid.tree().rank(l) < rank);
    links.get(at).filter(|(l, _)| *l == link).map(|_| at)
}

/// A link's first place: its first piece on the line it first lies on.
pub fn first_place(laid: &Laid, link: NodeId, line: usize) -> Option<Place> {
    let line = laid.lines().get(line)?;
    text::items(laid, line).find_map(|item| {
        let node = match item.piece {
            Piece::Text(i) => line.fragments.get(i).map(|f| f.node),
            Piece::Replaced(i) => line.replaced.get(i).map(|r| r.node),
        }?;
        (laid.tree().link(node) == Some(link)).then_some(item.start)
    })
}

/// The element a fragment names, laid: its first place, or the next
/// piece's after it in tree order.
pub fn target_place(laid: &Laid, target: NodeId) -> Option<Place> {
    let rank = laid.tree().rank(target);
    let at = text::line_of(
        laid,
        Place {
            rank,
            offset: 0,
            node: Some(target),
        },
    );
    // Lines are in the order of their starts, but a float's or an
    // inline-block's lie within the line it was met in, after it: the
    // least start at or after the target, among the lines from the
    // outermost one it lies in until one starts past the best.
    let at = text::outermost(laid, at);
    let mut best: Option<Place> = None;
    for line in laid.lines().get(at..)? {
        let start = text::bounds(laid, line).map(|(start, _)| start);
        if best.is_some_and(|best| start.is_some_and(|start| start > best)) {
            break;
        }
        let here = text::items(laid, line)
            .find(|item| item.start.rank >= rank)
            .map(|item| item.start);
        best = match (best, here) {
            (Some(best), Some(here)) => Some(best.min(here)),
            (best, here) => best.or(here),
        };
    }
    best
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::page::tests::{laid, WANT};
    use crate::pipeline::tests::measure;

    fn pane() -> Pane {
        Pane {
            rect: Rect {
                x: 0,
                y: 20,
                width: 400,
                height: 100,
            },
            inset: 8,
            scale: 1,
            line: 22,
        }
    }

    fn many() -> Laid {
        let mut markup = String::from("<body style=margin:0>");
        for n in 0..40 {
            markup.push_str(&format!("<p style=margin:0>line {n} <a href=/{n}>link</a>"));
        }
        laid(&markup)
    }

    #[test]
    fn scrolling_stays_within_the_page_and_the_top_is_kept() {
        let laid = many();
        let pane = pane();
        let mut view = View::default();
        view.scroll(&laid, &pane, -50, 0);
        assert_eq!(view.y, 0);
        view.end(&laid, &pane);
        assert_eq!(view.y, 40 * 16 - 100);
        view.y = 37;
        view.clamp(&laid, &pane);
        let anchor = view.anchor(&laid, &pane).unwrap();
        assert_eq!(anchor.dy, 5.0);
        view.home();
        view.restore(&laid, &pane, anchor);
        assert_eq!(view.y, 37);
        assert_eq!(view.bottom(&laid, &pane), (8, 40));
    }

    #[test]
    fn tab_steps_through_links_and_shows_them() {
        let laid = many();
        let pane = pane();
        let mut m = measure(WANT);
        let mut view = View {
            focus: Some(0),
            ..View::default()
        };
        view.step_link(&laid, &mut m, &pane, false);
        assert_eq!(view.focus, Some(39));
        assert!(view.y > 0);
        view.step_link(&laid, &mut m, &pane, true);
        assert_eq!(view.focus, Some(0));
        assert_eq!(view.y, 0);
        // The link under a point: line 0's link is at cells 7..11.
        let x = pane.rect.x + pane.inset + 8 * 8 + 2;
        assert_eq!(
            view.link_at(&laid, &mut m, &pane, x, pane.rect.y + 4),
            Some(0)
        );
        assert_eq!(
            view.link_at(
                &laid,
                &mut m,
                &pane,
                pane.rect.x + pane.inset + 2,
                pane.rect.y + 4
            ),
            None
        );
    }

    /// With no link focused, `Tab` starts at the first link on a line
    /// shown and `Shift+Tab` at the last, each going round the page.
    #[test]
    fn tab_starts_from_the_screen_and_goes_round() {
        let p = "<p style=margin:0>";
        let laid = laid(&format!(
            "<body style=margin:0>{p}<a href=/0>x</a>{p}a{p}a{p}<a href=/1>y</a>{p}<a href=/2>z</a>"
        ));
        let pane = Pane {
            rect: Rect {
                height: 32,
                ..pane().rect
            },
            ..pane()
        };
        let mut m = measure(WANT);
        let mut view = View {
            y: 32,
            ..View::default()
        };
        view.step_link(&laid, &mut m, &pane, true);
        assert_eq!(view.focus, Some(1), "the first link at or below the top");
        view.step_link(&laid, &mut m, &pane, true);
        view.step_link(&laid, &mut m, &pane, true);
        assert_eq!(view.focus, Some(0), "round");
        assert_eq!(view.y, 0, "revealed");
        let mut view = View::default();
        view.step_link(&laid, &mut m, &pane, false);
        assert_eq!(view.focus, Some(0), "the last link above the bottom");
        view.step_link(&laid, &mut m, &pane, false);
        assert_eq!(view.focus, Some(2), "round");
        assert_eq!(view.y, 5 * 16 - 32, "revealed at the bottom");
        // Below every link, Tab goes round to the first.
        let mut view = View {
            y: 48,
            ..View::default()
        };
        view.step_link(&laid, &mut m, &pane, true);
        assert_eq!(view.focus, Some(1));
        let laid = many();
        let mut view = View {
            y: 39 * 16,
            ..View::default()
        };
        view.step_link(&laid, &mut m, &pane, false);
        assert_eq!(view.focus, Some(39), "the last shown");
    }

    /// A page whose last line lays nothing, after a `br`: its last text
    /// line's link is under a point, its anchor kept and its target
    /// found.
    #[test]
    fn a_page_ending_in_a_line_with_nothing_on_it() {
        let laid = laid(
            "<body style=margin:0><p style=margin:0>a<p style=margin:0 id=t>\
             <a href=/x>link</a><br></p>",
        );
        assert!(text::items(&laid, laid.lines().last().unwrap())
            .next()
            .is_none());
        let pane = pane();
        let mut m = measure(WANT);
        let view = View::default();
        let x = pane.rect.x + pane.inset + 2;
        let y = pane.rect.y + 16 + 4;
        assert_eq!(view.link_at(&laid, &mut m, &pane, x, y), Some(0));
        let t = laid.tree().target("t").unwrap();
        let place = target_place(&laid, t).unwrap();
        assert_eq!(text::line_of(&laid, place), 1);
        assert_eq!(first_place(&laid, laid.links()[0].0, 1), Some(place));
        let anchor = Anchor { place, dy: 3.0 };
        let mut view = View::default();
        let tall = Pane {
            rect: Rect {
                height: 10,
                ..pane.rect
            },
            ..pane
        };
        view.restore(&laid, &tall, anchor);
        assert_eq!(view.y, 19);
    }

    /// A float met within a line: its lines follow that line, every
    /// place laid is found on the line that lays it, past the float's
    /// lines too, and the float is a fragment's target.
    #[test]
    fn places_in_and_around_a_float_met_within_a_line() {
        let laid = laid(
            "<body style=margin:0><p style=margin:0>one two \
             <span id=f style=float:right;width:60px>in the float</span> three four\
             <p style=margin:0>next",
        );
        let lines = laid.lines();
        let float = lines
            .iter()
            .position(|line| line.block() != lines[0].block())
            .unwrap();
        assert!(float > 0, "the float's lines follow the line it was met in");
        assert!(laid.outer(float).is_some());
        for (at, line) in lines.iter().enumerate() {
            for item in text::items(&laid, line) {
                assert_eq!(text::line_of(&laid, item.start), at, "{item:?}");
                assert_eq!(text::line_of(&laid, item.end), at, "{item:?}");
            }
        }
        let f = laid.tree().target("f").unwrap();
        let place = target_place(&laid, f).unwrap();
        assert_eq!(text::line_of(&laid, place), float);
        // Past the last piece of the line the float was met in: that line.
        let (_, end) = text::bounds(&laid, &lines[0]).unwrap();
        let past = Place {
            offset: end.offset + 1,
            ..end
        };
        assert_eq!(text::line_of(&laid, past), 0);
        // In white space a wrap dropped between two of a float's lines:
        // the first of them.
        let laid = crate::page::tests::laid(
            "<body style=margin:0><p style=margin:0>one two \
             <span style=float:right;width:40px>in    the float</span> three",
        );
        let lines = laid.lines();
        let float = lines
            .iter()
            .position(|line| line.block() != lines[0].block())
            .unwrap();
        let node = lines[float].fragments.first().unwrap().node;
        assert_eq!(text::line_of(&laid, Place::new(&laid, node, 4)), float);
        // A copy from inside the float to the end holds the tail of the
        // line it was met in; the whole page's end is that tail's.
        let copied = |from: Place, to: Place| {
            let mut out = String::new();
            let _ = text::copy(&laid, from, to, &mut |t| {
                out.push_str(t);
                std::ops::ControlFlow::Continue(())
            });
            out
        };
        let (first, last) = text::page_bounds(&laid);
        let (in_float, _) = text::bounds(&laid, &lines[float]).unwrap();
        let (_, float_end) = text::bounds(&laid, &lines[float + 2]).unwrap();
        // A selection ending in the float copies the float's text up to
        // its end.
        let to_float = copied(first, float_end);
        assert!(
            to_float.contains("in") && to_float.contains("float"),
            "{to_float}"
        );
        assert!(
            copied(in_float, last).contains("three"),
            "{}",
            copied(in_float, last)
        );
        let all = copied(first, last);
        assert!(
            ["one two", "in", "the", "float", "three"]
                .iter()
                .all(|w| all.contains(w)),
            "{all}"
        );
    }

    /// A link laid as an inline-block: a point on its text or its padding
    /// is on it, `Tab` reaches it in tree order, from the line holding it,
    /// and shows its place there.
    #[test]
    fn a_link_laid_as_an_inline_block() {
        let laid = laid(
            "<body style=margin:0><p style=margin:0>see <a href=/x \
             style=display:inline-block;padding:4px>the link</a> then <a href=/y>other</a>",
        );
        let lines = laid.lines();
        assert_eq!(laid.links().len(), 2);
        assert_eq!(laid.links()[0].1, 0);
        let pane = pane();
        let mut m = measure(WANT);
        let view = View::default();
        let window = |x: f64, y: f64| (pane.rect.x + pane.inset + x as i64, pane.rect.y + y as i64);
        let text = &lines[1];
        let (x, y) = window(text.fragments[0].x + 2.0, text.y + text.height / 2.0);
        assert_eq!(view.link_at(&laid, &mut m, &pane, x, y), Some(0));
        let r = &lines[0].replaced[0];
        let (x, y) = window(r.x + 1.0, r.y + 1.0);
        assert_eq!(view.link_at(&laid, &mut m, &pane, x, y), Some(0));
        let other = lines[0].fragments.last().unwrap();
        let (x, y) = window(other.x + other.width - 2.0, lines[0].y + 2.0);
        assert_eq!(view.link_at(&laid, &mut m, &pane, x, y), Some(1));
        let mut view = View::default();
        view.step_link(&laid, &mut m, &pane, true);
        assert_eq!(view.focus, Some(0));
        let (link, line) = laid.links()[0];
        let place = first_place(&laid, link, line as usize).unwrap();
        assert_eq!(place.node, Some(link));
        assert_eq!(text::line_of(&laid, place), 0);
        view.step_link(&laid, &mut m, &pane, true);
        assert_eq!(view.focus, Some(1));
    }

    /// An inline-block link after one whose last node is an inline-block:
    /// its padding is on it and its first place on the line holding it.
    #[test]
    fn a_link_after_a_nested_last_inline_block() {
        let laid = laid(
            "<body style=margin:0><div><a href=/a style=display:inline-block;padding:6px>\
             <span style=display:inline-block>icon</span></a><a href=/b \
             style=display:inline-block;padding:6px>Next</a></div>",
        );
        let lines = laid.lines();
        assert_eq!(laid.links().len(), 2);
        let pane = pane();
        let mut m = measure(WANT);
        let view = View::default();
        let window = |x: f64, y: f64| (pane.rect.x + pane.inset + x as i64, pane.rect.y + y as i64);
        let r = &lines[0].replaced[1];
        let (x, y) = window(r.x + 1.0, r.y + 1.0);
        assert_eq!(view.link_at(&laid, &mut m, &pane, x, y), Some(1));
        let (link, line) = laid.links()[1];
        let place = first_place(&laid, link, line as usize).unwrap();
        assert_eq!(text::line_of(&laid, place), 0);
    }

    /// An empty target between two floats' or inline-blocks' places in
    /// one line is the line's next piece after it, not a later box's.
    #[test]
    fn a_target_between_two_nested_boxes_of_a_line() {
        for markup in [
            "<p>foo<span style=display:inline-block>ab<br></span><a id=t></a>bar\
             <span style=float:left>F</span>baz",
            "<p>foo<span style=float:left>A</span><a id=t></a>bar<span style=float:left>B</span>baz",
        ] {
            let laid = laid(markup);
            let t = laid.tree().target("t").unwrap();
            let at = target_place(&laid, t).unwrap();
            let line = &laid.lines()[text::line_of(&laid, at)];
            let piece = text::items(&laid, line).find(|item| item.start == at).unwrap();
            let text = match piece.piece {
                Piece::Text(i) => text::fragment_text(&laid, &line.fragments[i]),
                Piece::Replaced(_) => "",
            };
            assert!(text.starts_with("bar"), "{markup}: {text:?}");
        }
    }

    #[test]
    fn a_fragment_target_is_its_first_place_or_the_next() {
        let laid = laid("<p>a<p id=t>b<p id=e><p>c");
        let t = laid.tree().target("t").unwrap();
        let at = target_place(&laid, t).unwrap();
        assert_eq!(text::line_of(&laid, at), 1);
        let empty = laid.tree().target("e").unwrap();
        let next = target_place(&laid, empty).unwrap();
        assert_eq!(text::line_of(&laid, next), 2);
    }

    /// Flex items' lines are in tree order, as table cells' are, an
    /// anonymous item's naming its container: every place is on the line
    /// laying it, items moved down included, and a copy parts the items'
    /// text as paragraphs.
    #[test]
    fn places_and_copies_across_flex_items() {
        let laid = laid(
            "<body style=margin:0><nav style=display:flex;align-items:center;column-gap:8px>\
             one <a href=/a>two<br>lines</a> three <b style=margin:auto>four</b>\
             <span style=display:flex;align-items:end><i>five</i><i>six<br>more</i></span> seven\
             </nav><p style=margin:0>after",
        );
        let lines = laid.lines();
        let shown = |line: &td_layout::inline::Line| {
            line.fragments
                .iter()
                .map(|f| text::fragment_text(&laid, f))
                .collect::<String>()
        };
        let texts: Vec<String> = lines.iter().map(shown).collect();
        assert_eq!(
            texts,
            ["one", "two", "lines", "three", "four", "five", "six", "more", "seven", "after"]
        );
        // Moved down: "one" centred beside the two-line link, "five" at
        // its container's end.
        assert!(lines[0].y > lines[1].y);
        assert!(lines[5].y > lines[6].y);
        for (at, line) in lines.iter().enumerate() {
            for item in text::items(&laid, line) {
                assert_eq!(text::line_of(&laid, item.start), at, "{item:?}");
                assert_eq!(text::line_of(&laid, item.end), at, "{item:?}");
            }
            assert_eq!(laid.outer(at), None);
        }
        // Two anonymous items name the container, and are two paragraphs.
        assert_eq!(lines[0].block(), lines[3].block());
        assert_eq!(text::paragraph(&laid, 0), (0, 0));
        assert_eq!(text::paragraph(&laid, 3), (3, 3));
        let mut out = String::new();
        let (first, last) = text::page_bounds(&laid);
        let _ = text::copy(&laid, first, last, &mut |t| {
            out.push_str(t);
            std::ops::ControlFlow::Continue(())
        });
        assert_eq!(
            out,
            "one\ntwo\nlines\nthree\nfour\nfive\nsix\nmore\nseven\nafter"
        );
    }

    /// A picture item is blockified, so it lays no fragment and has no
    /// place: its alt text is not copied, as a block picture's is not.
    #[test]
    fn a_picture_item_is_not_copied() {
        for (div, img) in [("display:flex", ""), ("", "display:block")] {
            let picture = laid(&format!(
                "<body style=margin:0><div style='{div}'>ab <img style='{img}' alt=IMG> cd</div>"
            ));
            let mut out = String::new();
            let (first, last) = text::page_bounds(&picture);
            let _ = text::copy(&picture, first, last, &mut |t| {
                out.push_str(t);
                std::ops::ControlFlow::Continue(())
            });
            assert_eq!(out, "ab\ncd", "{div} {img}");
        }
    }
}
