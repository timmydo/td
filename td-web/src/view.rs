//! Where the reader is in a rendered page: the first line shown, the
//! focused link and what find matched; and the page painted over its
//! pane in prose, each glyph at the pen its line was laid with.

use td_ui::prose::{LineBox, OVERHANG, SIZE};
use td_ui::raster::{
    fill, Draw, GlyphStyle, Primitive, Rect, ACCENT, CHROME, INACTIVE_SELECTION, INK, PAPER,
    SELECTED,
};

use crate::document::{Rendered, NO_LINK};

/// The most matches find marks.
pub const MAX_HITS: usize = 10_000;
/// The most glyphs drawn at one pen: combining marks take no advance in
/// Noto, so without a bound one line could ask a paint for as many glyphs
/// as the page has characters.
const MAX_STACKED: usize = 8;

/// A match: its line, and the pixels it spans from the line's left
/// (`left` inclusive, `right` exclusive).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hit {
    pub line: usize,
    pub left: u32,
    pub right: u32,
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct View {
    /// The first line shown.
    pub top: usize,
    /// The focused link.
    pub focus: Option<usize>,
    pub hits: Vec<Hit>,
    /// The current match, an index into `hits`.
    pub at: usize,
}

impl View {
    /// The first line of the last screenful.
    fn last_top(rendered: &Rendered, rows: usize) -> usize {
        rendered.lines.len().saturating_sub(rows.max(1))
    }

    /// Moves `delta` lines, kept inside the page.
    pub fn scroll(&mut self, delta: isize, rendered: &Rendered, rows: usize) {
        let top = self.top.saturating_add_signed(delta);
        self.top = top.min(Self::last_top(rendered, rows));
    }

    pub fn home(&mut self) {
        self.top = 0;
    }

    pub fn end(&mut self, rendered: &Rendered, rows: usize) {
        self.top = Self::last_top(rendered, rows);
    }

    /// Scrolls as little as shows `line`.
    pub fn reveal(&mut self, line: usize, rows: usize) {
        let rows = rows.max(1);
        if line < self.top {
            self.top = line;
        } else if line >= self.top.saturating_add(rows) {
            self.top = line.saturating_sub(rows - 1);
        }
    }

    /// Focuses the next link (`forward`) or the previous one: from the
    /// focused link, else from the screen, the first shown or the last.
    pub fn step_link(&mut self, forward: bool, rendered: &Rendered, rows: usize) {
        let count = rendered.page.links.len();
        if count == 0 {
            self.focus = None;
            return;
        }
        let next = match self.focus {
            Some(at) if forward => (at + 1) % count,
            Some(at) => (at + count - 1) % count,
            None if forward => rendered
                .link_lines
                .iter()
                .position(|&line| line >= self.top)
                .unwrap_or(0),
            None => {
                let bottom = self.top.saturating_add(rows);
                rendered
                    .link_lines
                    .iter()
                    .rposition(|&line| line < bottom)
                    .unwrap_or(count - 1)
            }
        };
        self.focus = Some(next);
        if let Some(&line) = rendered.link_lines.get(next) {
            self.reveal(line, rows);
        }
    }

    /// Marks every match of `query` (`mark`) and makes current the first
    /// at or after the screen's top, scrolling to it.
    pub fn find(&mut self, query: &str, rendered: &Rendered, rows: usize) {
        self.mark(query, rendered);
        self.at = self
            .hits
            .iter()
            .position(|hit| hit.line >= self.top)
            .unwrap_or(0);
        self.show_hit(rows);
    }

    /// Marks every match of `query` again on lines wrapped anew, keeping
    /// which is current as far as there are as many, and not scrolling.
    pub fn remark(&mut self, query: &str, rendered: &Rendered) {
        let at = self.at;
        self.mark(query, rendered);
        self.at = at.min(self.hits.len().saturating_sub(1));
    }

    /// Marks every match of `query` on a line, across its runs and
    /// ignoring case, at most `MAX_HITS`, each by the pixels its glyphs
    /// span.
    fn mark(&mut self, query: &str, rendered: &Rendered) {
        self.hits.clear();
        self.at = 0;
        let needle: Vec<char> = query.chars().map(fold).collect();
        if needle.is_empty() {
            return;
        }
        let mut hay: Vec<char> = Vec::new();
        'lines: for (line, wrapped) in rendered.lines.iter().enumerate() {
            let glyphs = &wrapped.line.glyphs;
            hay.clear();
            hay.extend(glyphs.iter().map(|glyph| fold(glyph.scalar)));
            let mut at = 0;
            while let Some(window) = hay.get(at..at + needle.len()) {
                if window == needle.as_slice() {
                    if self.hits.len() == MAX_HITS {
                        break 'lines;
                    }
                    let left = glyphs.get(at).map_or(0, |glyph| glyph.x);
                    let right = glyphs
                        .get(at + needle.len())
                        .map_or(wrapped.line.width, |glyph| glyph.x);
                    self.hits.push(Hit {
                        line,
                        left,
                        right: right.max(left),
                    });
                    at += needle.len();
                } else {
                    at += 1;
                }
            }
        }
    }

    /// Moves to the next match or the previous one, around the ends.
    pub fn step_hit(&mut self, forward: bool, rows: usize) {
        let count = self.hits.len();
        if count == 0 {
            return;
        }
        self.at = if forward {
            (self.at + 1) % count
        } else {
            (self.at + count - 1) % count
        };
        self.show_hit(rows);
    }

    fn show_hit(&mut self, rows: usize) {
        if let Some(hit) = self.hits.get(self.at) {
            self.reveal(hit.line, rows);
        }
    }

    /// The link painted at a point of `pane`: none in the inset left of
    /// the lines, and none on the line below the last shown whole.
    pub fn link_at(&self, rendered: &Rendered, pane: &Pane, x: i64, y: i64) -> Option<usize> {
        let rect = pane.rect;
        let left = rect.x.saturating_add(pane.inset);
        if !rect.contains(x, y) || x < left {
            return None;
        }
        let row = usize::try_from((y - rect.y) / pane.height()).ok()?;
        if row >= pane.shown() {
            return None;
        }
        let wrapped = rendered.lines.get(self.top.saturating_add(row))?;
        let piece = wrapped.line.hit(x - left)?;
        (piece.tag < rendered.page.links.len()).then_some(piece.tag)
    }
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
    let current = view.hits.get(view.at).copied();
    let lines = rendered.lines.iter().skip(view.top).take(pane.shown());
    for (row, wrapped) in lines.enumerate() {
        let index = view.top + row;
        let y = rect.y + row as i64 * height;
        // The line's matches: `hits` is in line order.
        let first = view.hits.partition_point(|hit| hit.line < index);
        let last = view.hits.partition_point(|hit| hit.line <= index);
        let hits = view.hits.get(first..last).unwrap_or_default();
        let line = &wrapped.line;
        for pass in [Pass::Ground, Pass::Glyph, Pass::Stroke] {
            // The line's matches in order, as its glyphs are: the first
            // not wholly left of the glyph.
            let mut next = 0;
            'pieces: for piece in &line.pieces {
                let Some(run) = rendered.page.run(wrapped, piece) else {
                    continue;
                };
                let link = (piece.tag != NO_LINK).then_some(piece.tag);
                let focused = link.is_some() && link == view.focus;
                let hovered = link.is_some() && link == hover;
                for at in piece.glyphs.clone() {
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
                    while hits.get(next).is_some_and(|hit| hit.right <= glyph.x) {
                        next += 1;
                    }
                    let hit = hits.get(next).filter(|hit| hit.left <= glyph.x).copied();
                    let ground = if focused || (hit.is_some() && hit == current) {
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
    use crate::document::{Link, Logical, Page, Run, Style};
    use std::sync::Arc;
    use td_ui::prose::Metrics;
    use td_ui::raster::{Scale, TextFamily};
    use td_ui::{CELL_HEIGHT, CELL_WIDTH};

    const CW: i64 = CELL_WIDTH as i64;
    const CH: i64 = CELL_HEIGHT as i64;

    fn cell() -> Metrics {
        Metrics::new(None, Scale::new(1).unwrap())
    }

    fn page(lines: &[&[(&str, Option<usize>)]], style: Style) -> Page {
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

    /// `lines` laid on the cell, 100 cells wide.
    fn rendered(lines: &[&[(&str, Option<usize>)]]) -> Rendered {
        let page = Arc::new(page(lines, Style::default()));
        Rendered::wrap(&page, &mut cell(), 800, TextFamily::Sans)
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

    #[test]
    fn scrolling_stays_inside_the_page() {
        let line: &[(&str, Option<usize>)] = &[("a", None)];
        let page = rendered(&[line; 10]);
        let mut view = View::default();
        view.scroll(-3, &page, 4);
        assert_eq!(view.top, 0);
        view.scroll(100, &page, 4);
        assert_eq!(view.top, 6);
        view.home();
        view.end(&page, 20);
        assert_eq!(view.top, 0, "a page shorter than the screen");
    }

    #[test]
    fn tab_starts_from_the_screen_and_goes_round() {
        let page = rendered(&[
            &[("x", Some(0))],
            &[("a", None)],
            &[("a", None)],
            &[("y", Some(1))],
            &[("z", Some(2))],
        ]);
        let mut view = View {
            top: 2,
            ..View::default()
        };
        view.step_link(true, &page, 2);
        assert_eq!(view.focus, Some(1));
        view.step_link(true, &page, 2);
        view.step_link(true, &page, 2);
        assert_eq!(view.focus, Some(0));
        assert_eq!(view.top, 0, "revealed");
        let mut view = View {
            top: 0,
            ..View::default()
        };
        view.step_link(false, &page, 2);
        assert_eq!(view.focus, Some(0), "the last link above the bottom");
    }

    #[test]
    fn find_ignores_case_and_steps_round_marking_pixels() {
        let page = rendered(&[
            &[("Alpha ", None), ("beta", Some(0))],
            &[("ALPHA alpha", None)],
        ]);
        let mut view = View::default();
        view.find("alpha", &page, 5);
        let w = CELL_WIDTH as u32;
        assert_eq!(
            view.hits,
            [
                Hit {
                    line: 0,
                    left: 0,
                    right: 5 * w
                },
                Hit {
                    line: 1,
                    left: 0,
                    right: 5 * w
                },
                Hit {
                    line: 1,
                    left: 6 * w,
                    right: 11 * w
                },
            ]
        );
        view.step_hit(false, 5);
        assert_eq!(view.at, 2);
        view.find("a b", &page, 5);
        assert_eq!(
            view.hits,
            [Hit {
                line: 0,
                left: 4 * w,
                right: 7 * w
            }],
            "across runs"
        );
        view.find("", &page, 5);
        assert!(view.hits.is_empty());
    }

    #[test]
    fn a_pixel_names_the_link_it_shows() {
        let page = rendered(&[&[("go ", None), ("here", Some(0))], &[("x", None)]]);
        let view = View::default();
        let at = |column: i64| CW + column * CW + 1;
        assert_eq!(view.link_at(&page, &pane(2), at(3), 1), Some(0));
        assert_eq!(view.link_at(&page, &pane(2), at(6), 1), Some(0));
        assert_eq!(view.link_at(&page, &pane(2), at(7), 1), None);
        assert_eq!(view.link_at(&page, &pane(2), at(1), 1), None);
        assert_eq!(view.link_at(&page, &pane(2), at(0), 5 * CH), None);
        let page = rendered(&[&[("here", Some(0))], &[("here", Some(0))]]);
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
        let page = rendered(&[&[("ab ", None), ("cd", Some(0))], &[("ab", None)]]);
        let mut view = View::default();
        view.find("b", &page, 2);
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
        let struck = Arc::new(page(&[&[("ab", None)]], struck));
        let rendered = Rendered::wrap(&struck, &mut cell(), 800, TextFamily::Sans);
        let (fills, _) = painted(&rendered, &View::default(), None, &pane(1));
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
        let code = Arc::new(page(&[&[("ab", None)]], code));
        let rendered = Rendered::wrap(&code, &mut cell(), 800, TextFamily::Sans);
        let (fills, glyphs) = painted(&rendered, &View::default(), None, &pane(1));
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
        let rendered = Rendered::wrap(&Arc::new(page), &mut cell(), 800, TextFamily::Sans);
        assert_eq!(rendered.lines.len(), 1);
        let (_, glyphs) = painted(&rendered, &View::default(), None, &pane(1));
        let last = glyphs.last().unwrap().0;
        assert!(last < 800 + i64::from(OVERHANG), "{last}");
        assert!(last >= 800, "{last}");
    }

    #[test]
    fn a_pile_of_marks_at_one_pen_draws_only_its_first() {
        let marks = "\u{301}".repeat(100);
        let mut page = page(&[&[(&marks, None)]], Style::default());
        page.lines[0].wrap = false;
        let mut rendered = Rendered::wrap(&Arc::new(page), &mut cell(), 800, TextFamily::Sans);
        // As Noto measures them: no advance.
        let line = &mut rendered.lines[0].line;
        for glyph in &mut line.glyphs {
            glyph.x = 0;
        }
        line.pieces[0].right = 0;
        line.width = 0;
        let (_, glyphs) = painted(&rendered, &View::default(), None, &pane(1));
        assert_eq!(glyphs.len(), MAX_STACKED);
    }
}
