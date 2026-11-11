//! Scalar wrapping with DOM-byte provenance. No retained glyph array.
use crate::{keyword, Error, Limits, StyleId, StyledDocument, MAX_COORDINATE};
use std::mem::size_of;
use std::ops::Range;
use td_css::values::Length;
use td_css::{ComputedStyle, ComputedValue, Property};
use td_html::document::NodeId;

pub const UNIT: u32 = 64;
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
pub struct Line {
    pub y: f64,
    pub height: f64,
    pub baseline: f64,
    pub width: f64,
    pub fragments: Vec<TextFragment>,
}
pub struct WrappedText {
    pub lines: Vec<Line>,
    pub charged_bytes: usize,
    pub work: usize,
}
#[derive(Clone)]
pub(crate) struct Pending {
    node: NodeId,
    range: Range<usize>,
    style: StyleId,
    display: DisplayText,
    pen: u64,
    advance: u64,
    above: i64,
    below: i64,
    space: bool,
}
pub(crate) struct Counters {
    pub bytes: usize,
    pub work: usize,
    pub lines: usize,
    pub fragments: usize,
    pub limits: Limits,
}
impl Counters {
    pub fn new(styled: &StyledDocument<'_>) -> Self {
        Self {
            bytes: styled.charged_bytes(),
            work: 0,
            lines: 0,
            fragments: 0,
            limits: styled.limits(),
        }
    }
    pub fn step(&mut self, count: usize) -> Result<(), Error> {
        self.work = self
            .work
            .checked_add(count)
            .filter(|n| *n <= self.limits.work)
            .ok_or(Error::Limit("MAX_LAYOUT_WORK"))?;
        Ok(())
    }
    pub fn reserve<T>(&mut self, values: &mut Vec<T>) -> Result<(), Error> {
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
            values
                .try_reserve_exact(capacity - values.len())
                .map_err(|_| Error::Limit("MAX_PAGE_BYTES"))?;
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
pub(crate) trait LineSink {
    fn wants(&self, y: f64, height: f64) -> bool;
    fn line(&mut self, line: Line, counters: &mut Counters) -> Result<(), Error>;
}
struct AllLines(Vec<Line>);
impl LineSink for AllLines {
    fn wants(&self, _: f64, _: f64) -> bool {
        true
    }
    fn line(&mut self, line: Line, counters: &mut Counters) -> Result<(), Error> {
        counters.reserve(&mut self.0)?;
        self.0.push(line);
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
    let mut counters = Counters::new(styled);
    counters.step(runs.len())?;
    let mut sink = AllLines(Vec::new());
    let mut flow = Flow::new(styled, metrics, width, strut, 0.0)?;
    for run in runs {
        flow.text(run, &mut counters, &mut sink)?;
    }
    flow.finish(&mut counters, &mut sink)?;
    Ok(WrappedText {
        lines: sink.0,
        charged_bytes: counters.bytes,
        work: counters.work,
    })
}
pub(crate) struct Flow<'s, 'd, 'm, M> {
    styled: &'s StyledDocument<'d>,
    metrics: &'m mut M,
    width: u32,
    pending: Vec<Pending>,
    pen: u64,
    after_space: Option<usize>,
    last_space: bool,
    after_newline: bool,
    previous_cr: bool,
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
    ) -> Result<Self, Error> {
        if f64::from(width) > MAX_COORDINATE || !coordinate(y) {
            return Err(Error::Invalid);
        }
        let style = styled.style(strut).ok_or(Error::Invalid)?;
        let (above, below) = vertical(metrics, style)?;
        Ok(Self {
            styled,
            metrics,
            width,
            pending: Vec::new(),
            pen: 0,
            after_space: None,
            last_space: true,
            after_newline: false,
            previous_cr: false,
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
            .and_then(|s| s.get(run.range.clone()))
            .ok_or(Error::Invalid)?;
        let style = self.styled.style(run.style).ok_or(Error::Invalid)?;
        let whitespace = keyword(style, Property::WhiteSpace);
        let preserve = matches!(whitespace, "pre" | "pre-wrap");
        let newline = preserve || whitespace == "pre-line";
        let soft = !matches!(whitespace, "pre" | "nowrap");
        let (above, below) = vertical(self.metrics, style)?;
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
            let (shown, display) = if (!preserve && ascii_space) || scalar == '\t' {
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
            };
            self.last_space = !preserve && ascii_space;
            let spacing = match style.get(Property::LetterSpacing) {
                Some(ComputedValue::Length(Length::Px(n))) => (*n * f64::from(UNIT)).round(),
                _ => 0.0,
            };
            let advance = (f64::from(self.metrics.advance(style, shown)) + spacing).max(0.0) as u64;
            let item = Pending {
                node: run.node,
                range: run.range.start + at..run.range.start + at + scalar.len_utf8(),
                style: run.style,
                display,
                pen: 0,
                advance,
                above,
                below,
                space: shown == ' ',
            };
            self.push(item, soft, counters, sink)?;
        }
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
        self.pending.clear();
        self.pen = 0;
        self.after_space = None;
        self.last_space = true;
        self.after_newline = true;
        self.previous_cr = false;
        Ok(())
    }
    fn push(
        &mut self,
        mut item: Pending,
        soft: bool,
        counters: &mut Counters,
        sink: &mut impl LineSink,
    ) -> Result<(), Error> {
        let width = self.width;
        let over = |pen: u64| (pen + item.advance + 32) / 64 > u64::from(width);
        if !item.space
            && over(self.pen)
            && !self.pending.is_empty()
            && (soft || self.after_space.is_some())
        {
            let rest = match self.after_space.take() {
                Some(keep) => self.pending.split_off(keep.min(self.pending.len())),
                None => Vec::new(),
            };
            if self.pending.iter().any(|p| !p.space) {
                self.emit(counters, sink, None)?;
            }
            let shift = rest.first().map_or(self.pen, |p| p.pen);
            self.pending = rest;
            counters.step(self.pending.len())?;
            for p in &mut self.pending {
                p.pen -= shift;
            }
            self.pen -= shift.min(self.pen);
            if soft && over(self.pen) && !self.pending.is_empty() {
                self.emit(counters, sink, None)?;
                self.pending.clear();
                self.pen = 0;
            }
        }
        if self.pending.len() >= counters.limits.line_scalars {
            return Err(Error::Limit("MAX_LINE_SCALARS"));
        }
        item.pen = self.pen;
        self.pen = self.pen.checked_add(item.advance).ok_or(Error::Invalid)?;
        if !coordinate(self.pen as f64 / 64.0) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        let space = item.space;
        self.pending.push(item);
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
        self.pending.clear();
        self.pen = 0;
        self.after_newline = false;
        Ok(())
    }
    fn emit(
        &mut self,
        counters: &mut Counters,
        sink: &mut impl LineSink,
        extra: Option<(i64, i64)>,
    ) -> Result<(), Error> {
        counters.step(self.pending.len())?;
        if counters.lines >= counters.limits.lines {
            return Err(Error::Limit("MAX_LINES"));
        }
        counters.lines += 1;
        let end = self
            .pending
            .iter()
            .rposition(|p| !p.space)
            .map_or(0, |i| i + 1);
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
        if sink.wants(self.y, height) {
            let mut fragments: Vec<TextFragment> = Vec::new();
            for p in pending {
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
                    counters.reserve(&mut fragments)?;
                    fragments.push(TextFragment {
                        node: p.node,
                        range: p.range.clone(),
                        style: p.style,
                        display: p.display,
                        x: p.pen as f64 / 64.0,
                        width: p.advance as f64 / 64.0,
                    });
                }
            }
            sink.line(
                Line {
                    y: self.y,
                    height,
                    baseline: above as f64 / 64.0,
                    width,
                    fragments,
                },
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
fn vertical(metrics: &mut impl FontMetrics, style: &ComputedStyle) -> Result<(i64, i64), Error> {
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
            ("a\tb", 3, vec!["a b"]),
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
        for (limit, ok) in [(2, true), (1, false)] {
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
            assert_eq!(
                wrap_text(&styled, &mut Metrics(64), &runs, 10, runs[0].style).is_ok(),
                ok
            );
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
    fn reference(text: &str, width: usize) -> Vec<String> {
        let capacity = width.max(1);
        let mut output = Vec::new();
        for paragraph in text.split('\n') {
            let chars: Vec<_> = paragraph
                .chars()
                .map(|c| if c == '\t' { ' ' } else { c })
                .collect();
            let mut line = String::new();
            let mut spaces = 0;
            let mut at = 0;
            while at < chars.len() {
                if chars[at] == ' ' {
                    spaces += 1;
                    at += 1;
                    continue;
                }
                let begin = at;
                while at < chars.len() && chars[at] != ' ' {
                    at += 1;
                }
                let word = &chars[begin..at];
                let fitting = line.chars().count() + spaces + word.len() <= width;
                if !fitting {
                    if !line.is_empty() {
                        output.push(std::mem::take(&mut line));
                    }
                    spaces = 0;
                }
                line.extend(std::iter::repeat_n(' ', spaces));
                spaces = 0;
                for scalar in word {
                    if line.chars().count() == capacity {
                        output.push(std::mem::take(&mut line));
                    }
                    line.push(*scalar);
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
        for (count, ok) in [
            (crate::MAX_LINE_SCALARS, true),
            (crate::MAX_LINE_SCALARS + 1, false),
        ] {
            let html = format!("<p>{}", "a".repeat(count));
            let dom = td_html::document::parse(&html).unwrap();
            let styled = StyledDocument::new(&dom, &sheet, html.len(), Limits::default()).unwrap();
            let runs = runs(&styled);
            assert_eq!(
                wrap_text(&styled, &mut Metrics(0), &runs, 0, runs[0].style).is_ok(),
                ok
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
