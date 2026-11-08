//! Where the reader is in a rendered page: the first line shown, the
//! focused link and what find matched; and the page painted over its
//! rectangle, one cell a character.

use td_ui::raster::{
    fill, text_run, Draw, GlyphStyle, Rect, Scale, ACCENT, CHROME, INACTIVE_SELECTION, INK, PAPER,
    SELECTED,
};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

use crate::document::Rendered;

/// The most matches find marks.
pub const MAX_HITS: usize = 10_000;

/// A match: its line, its first column and its length, in characters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hit {
    pub line: usize,
    pub column: usize,
    pub len: usize,
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
        let count = rendered.links.len();
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

    /// Marks every match of `query`, ignoring case, at most `MAX_HITS`,
    /// and makes current the first at or after the screen's top.
    pub fn find(&mut self, query: &str, rendered: &Rendered, rows: usize) {
        self.hits.clear();
        self.at = 0;
        let needle: Vec<char> = query.chars().map(fold).collect();
        if needle.is_empty() {
            return;
        }
        let mut hay: Vec<char> = Vec::new();
        'lines: for (line, runs) in rendered.lines.iter().enumerate() {
            hay.clear();
            hay.extend(runs.iter().flat_map(|run| run.text.chars()).map(fold));
            let mut column = 0;
            while let Some(window) = hay.get(column..column + needle.len()) {
                if window == needle.as_slice() {
                    if self.hits.len() == MAX_HITS {
                        break 'lines;
                    }
                    self.hits.push(Hit {
                        line,
                        column,
                        len: needle.len(),
                    });
                    column += needle.len();
                } else {
                    column += 1;
                }
            }
        }
        self.at = self
            .hits
            .iter()
            .position(|hit| hit.line >= self.top)
            .unwrap_or(0);
        self.show_hit(rows);
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

    /// The link under a cell, its row counted from the screen's top, of
    /// `rows` shown whole.
    pub fn link_at(
        &self,
        rendered: &Rendered,
        rows: usize,
        row: usize,
        column: usize,
    ) -> Option<usize> {
        if row >= rows {
            return None;
        }
        let runs = rendered.lines.get(self.top.saturating_add(row))?;
        let mut start = 0;
        for run in runs {
            let end = start + run.text.chars().count();
            if (start..end).contains(&column) {
                return run.link;
            }
            start = end;
        }
        None
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

/// Paints `rendered` as `view` shows it inside `rect`, a cell's inset
/// left; `hover` is the link under the pointer.
pub fn paint(
    rendered: &Rendered,
    view: &View,
    hover: Option<usize>,
    rect: Rect,
    scale: Scale,
    sink: &mut dyn FnMut(Draw),
) {
    fill(rect, PAPER, rect, sink);
    let s = scale.value() as i64;
    let cw = CELL_WIDTH as i64 * s;
    let ch = CELL_HEIGHT as i64 * s;
    let rows = (i64::from(rect.height) / ch).max(0) as usize;
    let columns = (i64::from(rect.width) / cw).saturating_sub(1).max(0) as usize;
    let current = view.hits.get(view.at).copied();
    for (row, runs) in rendered.lines.iter().skip(view.top).take(rows).enumerate() {
        let line = view.top + row;
        let y = rect.y + row as i64 * ch;
        // The line's matches: `hits` is in line order.
        let first = view.hits.partition_point(|hit| hit.line < line);
        let last = view.hits.partition_point(|hit| hit.line <= line);
        let hits = view.hits.get(first..last).unwrap_or_default();
        let mut column = 0usize;
        'runs: for run in runs {
            let focused = run.link.is_some() && run.link == view.focus;
            let hovered = run.link.is_some() && run.link == hover;
            for scalar in run.text.chars() {
                if column >= columns {
                    break 'runs;
                }
                let x = rect.x + cw + column as i64 * cw;
                let hit = hits
                    .iter()
                    .find(|hit| (hit.column..hit.column + hit.len).contains(&column))
                    .copied();
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
                } else if run.link.is_some() {
                    ACCENT
                } else {
                    INK
                };
                let cell = Rect {
                    x,
                    y,
                    width: cw as u32,
                    height: ch as u32,
                };
                if ground != PAPER {
                    fill(cell, ground, rect, sink);
                }
                let mut style = GlyphStyle::medium(ink, ground);
                if run.style.bold {
                    style = style.bold();
                }
                if run.style.italic {
                    style = style.italic();
                }
                text_run(
                    scale,
                    std::iter::once(scalar),
                    (x, y),
                    rect,
                    style,
                    rect,
                    sink,
                );
                if run.style.strike {
                    let strike = Rect {
                        y: y + ch / 2,
                        height: s as u32,
                        ..cell
                    };
                    fill(strike, ink, rect, sink);
                }
                if run.link.is_some() && (hovered || ground != SELECTED) {
                    let underline = Rect {
                        y: y + ch - s,
                        height: s as u32,
                        ..cell
                    };
                    fill(underline, ink, rect, sink);
                }
                column += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::document::{Link, Run, Style};

    fn rendered(lines: &[&[(&str, Option<usize>)]]) -> Rendered {
        let mut out = Rendered::default();
        for (number, line) in lines.iter().enumerate() {
            let mut runs = Vec::new();
            for &(text, link) in line.iter() {
                if let Some(at) = link {
                    if out.links.len() == at {
                        out.links.push(Link {
                            written: format!("/{at}"),
                        });
                        out.link_lines.push(number);
                    }
                }
                runs.push(Run {
                    text: text.into(),
                    style: Style::default(),
                    link,
                });
            }
            out.lines.push(runs);
        }
        out
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
    fn find_ignores_case_and_steps_round() {
        let page = rendered(&[
            &[("Alpha ", None), ("beta", Some(0))],
            &[("ALPHA alpha", None)],
        ]);
        let mut view = View::default();
        view.find("alpha", &page, 5);
        assert_eq!(
            view.hits,
            [
                Hit {
                    line: 0,
                    column: 0,
                    len: 5
                },
                Hit {
                    line: 1,
                    column: 0,
                    len: 5
                },
                Hit {
                    line: 1,
                    column: 6,
                    len: 5
                },
            ]
        );
        view.step_hit(false, 5);
        assert_eq!(view.at, 2);
        view.find("a b", &page, 5);
        assert_eq!(view.hits.len(), 1, "across runs");
        view.find("", &page, 5);
        assert!(view.hits.is_empty());
    }

    #[test]
    fn a_cell_names_the_link_it_shows() {
        let page = rendered(&[&[("go ", None), ("here", Some(0))], &[("x", None)]]);
        let view = View::default();
        assert_eq!(view.link_at(&page, 2, 0, 3), Some(0));
        assert_eq!(view.link_at(&page, 2, 0, 6), Some(0));
        assert_eq!(view.link_at(&page, 2, 0, 7), None);
        assert_eq!(view.link_at(&page, 2, 0, 1), None);
        assert_eq!(view.link_at(&page, 2, 5, 0), None);
        let page = rendered(&[&[("a", None)], &[("here", Some(0))]]);
        assert_eq!(view.link_at(&page, 2, 1, 0), Some(0));
        assert_eq!(view.link_at(&page, 1, 1, 0), None, "a row not shown whole");
    }
}
