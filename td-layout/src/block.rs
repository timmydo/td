//! Normal-flow geometry and windowed line retention.
use crate::inline::{
    coordinate, Counters, Flow, FontMetrics, ImageRun, Line, LineBounds, LineSink, TextRun,
};
use crate::{keyword, BoxId, BoxKind, BoxNode, Error, StyleId, StyledDocument};
use std::ops::Range;
use td_css::values::Length;
use td_css::{ComputedStyle, ComputedValue, Property};
use td_html::document::NodeId;

pub const MAX_VIEWPORT: f64 = td_css::values::MAX_ABSOLUTE_VALUE;
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Edges {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}
impl Edges {
    fn horizontal(self) -> f64 {
        self.left + self.right
    }
    fn vertical(self) -> f64 {
        self.top + self.bottom
    }
    fn plus(self, other: Self) -> Self {
        Self {
            top: self.top + other.top,
            right: self.right + other.right,
            bottom: self.bottom + other.bottom,
            left: self.left + other.left,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Request {
    pub width: f64,
    pub height: f64,
    pub window: Option<Range<f64>>,
    /// Old layout output kept alive during this call, excluding shared input.
    pub retained_output_bytes: usize,
}
impl Request {
    pub fn new(width: f64, height: f64) -> Self {
        Self {
            width,
            height,
            window: None,
            retained_output_bytes: 0,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxGeometry {
    pub node: NodeId,
    pub style: StyleId,
    pub content: Rect,
    pub border: Rect,
    pub padding: Edges,
    pub border_widths: Edges,
    pub margin: Edges,
    pub replaced: bool,
}
pub struct Layout {
    pub boxes: Vec<BoxGeometry>,
    pub lines: Vec<Line>,
    pub height: f64,
    pub overflow: Rect,
    pub line_count: usize,
    pub work: usize,
    pub charged_bytes: usize,
    pub output_bytes: usize,
}
pub fn layout(
    styled: &StyledDocument<'_>,
    metrics: &mut impl FontMetrics,
    request: &Request,
) -> Result<Layout, Error> {
    for n in [request.width, request.height] {
        if !n.is_finite() || !(0.0..=MAX_VIEWPORT).contains(&n) {
            return Err(Error::Limit("MAX_VIEWPORT"));
        }
    }
    if request
        .window
        .as_ref()
        .is_some_and(|w| !coordinate(w.start) || !coordinate(w.end) || w.start > w.end)
    {
        return Err(Error::Invalid);
    }
    let mut counters = Counters::new(styled);
    counters.charge(request.retained_output_bytes)?;
    let initial = counters.bytes;
    let mut engine = Engine {
        styled,
        metrics,
        counters,
        request,
        boxes: Vec::new(),
        lines: Vec::new(),
        left: 0.0,
        top: 0.0,
        right: request.width,
        bottom: 0.0,
    };
    let height = if let Some(first) = styled.first_box() {
        let strut = styled.box_node(first).ok_or(Error::Invalid)?.style;
        engine.children(
            Some(first),
            Rect {
                x: 0.0,
                y: 0.0,
                width: request.width,
                height: request.height,
            },
            Some(request.height),
            (0.0, 0.0),
            0,
            strut,
        )?
    } else {
        0.0
    };
    Ok(Layout {
        boxes: engine.boxes,
        lines: engine.lines,
        height: height.max(engine.bottom),
        overflow: Rect {
            x: engine.left,
            y: engine.top,
            width: engine.right - engine.left,
            height: engine.bottom - engine.top,
        },
        line_count: engine.counters.lines,
        work: engine.counters.work,
        charged_bytes: engine.counters.bytes,
        output_bytes: engine.counters.bytes - initial,
    })
}
struct Engine<'s, 'd, 'm, 'r, M> {
    styled: &'s StyledDocument<'d>,
    metrics: &'m mut M,
    counters: Counters,
    request: &'r Request,
    boxes: Vec<BoxGeometry>,
    lines: Vec<Line>,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}
#[derive(Clone, Copy)]
enum Event {
    Inline(BoxId),
    Block(BoxId),
}
#[derive(Clone, Copy)]
struct Dimensions {
    width: f64,
    height: Option<f64>,
    margin: Edges,
    padding: Edges,
    border: Edges,
    min_height: f64,
    max_height: f64,
    relative: (f64, f64),
}
impl Dimensions {
    fn decoration(self) -> Edges {
        self.padding.plus(self.border)
    }
}
impl<M: FontMetrics> Engine<'_, '_, '_, '_, M> {
    fn bounds(&mut self, rect: Rect) -> Result<(), Error> {
        include_bounds(
            &mut self.left,
            &mut self.top,
            &mut self.right,
            &mut self.bottom,
            rect,
        )
    }
    fn events(&mut self, first: Option<BoxId>) -> Result<Vec<Event>, Error> {
        let mut events = Vec::new();
        let mut stack = Vec::new();
        let mut next = first;
        while let Some(id) = next {
            self.counters.step(1)?;
            let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
            if b.kind == BoxKind::Inline {
                if let Some(child) = b.first_child {
                    if let Some(sibling) = b.next_sibling {
                        stack.push(sibling);
                    }
                    next = Some(child);
                    continue;
                }
            } else {
                self.counters.reserve(&mut events)?;
                let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
                let block = b.kind == BoxKind::Block
                    || (b.kind == BoxKind::Image
                        && !matches!(keyword(style, Property::Display), "inline" | "inline-block"));
                events.push(if block {
                    Event::Block(id)
                } else {
                    Event::Inline(id)
                });
            }
            next = b.next_sibling.or_else(|| stack.pop());
        }
        Ok(events)
    }
    fn children(
        &mut self,
        first: Option<BoxId>,
        container: Rect,
        definite: Option<f64>,
        offset: (f64, f64),
        depth: usize,
        strut: StyleId,
    ) -> Result<f64, Error> {
        if depth > self.counters.limits.depth {
            return Err(Error::Limit("MAX_DEPTH"));
        }
        let events = self.events(first)?;
        let mut at = 0;
        let mut y = container.y;
        let mut previous_margin: Option<f64> = None;
        while let Some(event) = events.get(at).copied() {
            match event {
                Event::Block(id) => {
                    let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
                    let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
                    let dims = dimensions(self.styled, &b, style, container.width, definite)?;
                    let gap = previous_margin.map_or(dims.margin.top, |previous| {
                        collapse(previous, dims.margin.top)
                    });
                    y += gap;
                    y += self.block(b, dims, container.x, y, offset, depth + 1)?;
                    previous_margin = Some(dims.margin.bottom);
                    at += 1;
                }
                Event::Inline(_) => {
                    let start = at;
                    while matches!(events.get(at), Some(Event::Inline(_))) {
                        at += 1;
                    }
                    let run_y = y + previous_margin.unwrap_or(0.0);
                    let before = self.counters.lines;
                    let style = self.styled.style(strut).ok_or(Error::Invalid)?;
                    let align = keyword(style, Property::TextAlign);
                    let mut sink = Positioned {
                        lines: &mut self.lines,
                        window: self.request.window.as_ref(),
                        x: container.x + offset.0,
                        dy: offset.1,
                        width: container.width,
                        align,
                        left: &mut self.left,
                        top: &mut self.top,
                        right: &mut self.right,
                        bottom: &mut self.bottom,
                    };
                    let mut flow = Flow::new(
                        self.styled,
                        self.metrics,
                        container.width.floor() as u32,
                        strut,
                        run_y,
                    )?;
                    for event in events.get(start..at).ok_or(Error::Invalid)? {
                        let Event::Inline(id) = event else {
                            return Err(Error::Invalid);
                        };
                        let b = self.styled.box_node(*id).ok_or(Error::Invalid)?;
                        match b.kind {
                            BoxKind::Text => {
                                let text = self
                                    .styled
                                    .document()
                                    .node(b.node)
                                    .and_then(|n| n.text())
                                    .ok_or(Error::Invalid)?;
                                flow.text(
                                    &TextRun {
                                        node: b.node,
                                        style: b.style,
                                        range: 0..text.len(),
                                    },
                                    &mut self.counters,
                                    &mut sink,
                                )?;
                            }
                            BoxKind::Break => {
                                flow.hard_break(b.style, &mut self.counters, &mut sink)?
                            }
                            BoxKind::Image => {
                                let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
                                let dims =
                                    dimensions(self.styled, b, style, container.width, definite)?;
                                let decor = dims.decoration();
                                flow.image(
                                    ImageRun {
                                        node: b.node,
                                        style: b.style,
                                        width: dims.width + decor.horizontal(),
                                        height: dims.height.unwrap_or(0.0) + decor.vertical(),
                                        margin: [
                                            dims.margin.top,
                                            dims.margin.right,
                                            dims.margin.bottom,
                                            dims.margin.left,
                                        ],
                                    },
                                    &mut self.counters,
                                    &mut sink,
                                )?;
                            }
                            _ => return Err(Error::Invalid),
                        }
                    }
                    flow.finish(&mut self.counters, &mut sink)?;
                    let end_y = flow.y;
                    if self.counters.lines != before {
                        y = end_y;
                        previous_margin = None;
                    }
                }
            }
            if !coordinate(y) {
                return Err(Error::Limit("MAX_COORDINATE"));
            }
        }
        let end = y + previous_margin.unwrap_or(0.0);
        let height = (end - container.y).max(0.0);
        if !coordinate(end) || !coordinate(height) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        Ok(height)
    }
    fn block(
        &mut self,
        b: BoxNode,
        dims: Dimensions,
        x: f64,
        y: f64,
        offset: (f64, f64),
        depth: usize,
    ) -> Result<f64, Error> {
        self.counters.step(1)?;
        let decor = dims.decoration();
        let x = x + dims.margin.left;
        let content_x = x + decor.left;
        let content_y = y + decor.top;
        let mut offset = offset;
        offset.0 += dims.relative.0;
        offset.1 += dims.relative.1;
        let natural = if b.kind == BoxKind::Image {
            dims.height.unwrap_or(0.0)
        } else {
            self.children(
                b.first_child,
                Rect {
                    x: content_x,
                    y: content_y,
                    width: dims.width,
                    height: dims.height.unwrap_or(0.0),
                },
                dims.height,
                offset,
                depth,
                b.style,
            )?
        };
        let height = dims
            .height
            .unwrap_or(natural)
            .min(dims.max_height)
            .max(dims.min_height);
        let geometry = BoxGeometry {
            node: b.node,
            style: b.style,
            content: Rect {
                x: content_x + offset.0,
                y: content_y + offset.1,
                width: dims.width,
                height,
            },
            border: Rect {
                x: x + offset.0,
                y: y + offset.1,
                width: dims.width + decor.horizontal(),
                height: height + decor.vertical(),
            },
            padding: dims.padding,
            border_widths: dims.border,
            margin: dims.margin,
            replaced: b.kind == BoxKind::Image,
        };
        self.bounds(geometry.border)?;
        self.counters.reserve(&mut self.boxes)?;
        self.boxes.push(geometry);
        Ok(height + decor.vertical())
    }
}
struct Positioned<'a> {
    lines: &'a mut Vec<Line>,
    window: Option<&'a Range<f64>>,
    x: f64,
    dy: f64,
    width: f64,
    align: &'a str,
    left: &'a mut f64,
    top: &'a mut f64,
    right: &'a mut f64,
    bottom: &'a mut f64,
}
impl LineSink for Positioned<'_> {
    fn measure(&mut self, bounds: LineBounds) -> Result<(), Error> {
        let shift = self.x + alignment(self.align, self.width - bounds.advance);
        include_bounds(
            self.left,
            self.top,
            self.right,
            self.bottom,
            Rect {
                x: shift + bounds.left,
                y: self.dy + bounds.top,
                width: bounds.right - bounds.left,
                height: bounds.bottom - bounds.top,
            },
        )
    }
    fn wants(&self, y: f64, height: f64) -> bool {
        self.window.is_none_or(|w| {
            w.start < w.end && y + self.dy + height > w.start && y + self.dy < w.end
        })
    }
    fn line(&mut self, mut line: Line, counters: &mut Counters) -> Result<(), Error> {
        let shift = self.x + alignment(self.align, self.width - line.width);
        let local_y = line.y;
        line.y += self.dy;
        for f in &mut line.fragments {
            f.x += shift;
            if !coordinate(f.x + f.width) {
                return Err(Error::Limit("MAX_COORDINATE"));
            }
        }
        for f in &mut line.replaced {
            f.x += shift;
            f.y = (local_y + f.y) + self.dy;
            if !coordinate(f.x + f.width) || !coordinate(f.y + f.height) {
                return Err(Error::Limit("MAX_COORDINATE"));
            }
        }
        counters.reserve(self.lines)?;
        self.lines.push(line);
        Ok(())
    }
}
fn length(style: &ComputedStyle, p: Property, basis: Option<f64>) -> Option<f64> {
    match style.get(p) {
        Some(ComputedValue::Length(Length::Px(n))) => Some(*n),
        Some(ComputedValue::Length(Length::Percent(percent))) => {
            basis.map(|basis| (percent / 100.0 * basis).clamp(-MAX_VIEWPORT, MAX_VIEWPORT))
        }
        Some(ComputedValue::Length(Length::Calc {
            px,
            percent,
            non_negative,
        })) => basis.map(|basis| {
            (px + percent / 100.0 * basis).clamp(
                if *non_negative { 0.0 } else { -MAX_VIEWPORT },
                MAX_VIEWPORT,
            )
        }),
        _ => None,
    }
}
fn edges(style: &ComputedStyle, properties: [Property; 4], basis: f64, nonnegative: bool) -> Edges {
    let mut values = properties.map(|p| length(style, p, Some(basis)).unwrap_or(0.0));
    if nonnegative {
        values = values.map(|v| v.max(0.0));
    }
    let [top, right, bottom, left] = values;
    Edges {
        top,
        right,
        bottom,
        left,
    }
}
fn dimensions(
    styled: &StyledDocument<'_>,
    b: &BoxNode,
    style: &ComputedStyle,
    width: f64,
    height: Option<f64>,
) -> Result<Dimensions, Error> {
    use Property::*;
    let padding = edges(
        style,
        [PaddingTop, PaddingRight, PaddingBottom, PaddingLeft],
        width,
        true,
    );
    let border = edges(
        style,
        [
            BorderTopWidth,
            BorderRightWidth,
            BorderBottomWidth,
            BorderLeftWidth,
        ],
        width,
        true,
    );
    let mut margin = edges(
        style,
        [MarginTop, MarginRight, MarginBottom, MarginLeft],
        width,
        false,
    );
    let decor = padding.plus(border);
    let border_box = keyword(style, BoxSizing) == "border-box";
    let content = |value: f64, decoration: f64| {
        if border_box {
            (value - decoration).max(0.0)
        } else {
            value.max(0.0)
        }
    };
    let specified_width = length(style, Width, Some(width)).map(|v| content(v, decor.horizontal()));
    let specified_height = length(style, Height, height).map(|v| content(v, decor.vertical()));
    let mut used_width =
        specified_width.unwrap_or((width - margin.horizontal() - decor.horizontal()).max(0.0));
    let mut used_height = specified_height;
    if b.kind == BoxKind::Image {
        let element = styled
            .document()
            .node(b.node)
            .and_then(|n| n.element())
            .ok_or(Error::Invalid)?;
        let attribute = |name| {
            element
                .attribute(name)
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|n| n.is_finite() && (0.0..=MAX_VIEWPORT).contains(n))
        };
        let attr_width = attribute("width");
        let attr_height = attribute("height");
        let ratio = attr_width
            .zip(attr_height)
            .filter(|(w, h)| *w > 0.0 && *h > 0.0)
            .map(|(w, h)| w / h);
        used_width = specified_width.unwrap_or_else(|| {
            specified_height
                .zip(ratio)
                .map(|(h, r)| h * r)
                .unwrap_or(attr_width.unwrap_or(300.0))
        });
        used_height = Some(specified_height.unwrap_or_else(|| {
            specified_width
                .zip(ratio)
                .map(|(w, r)| w / r)
                .unwrap_or(attr_height.unwrap_or(150.0))
        }));
    }
    let min = length(style, MinWidth, Some(width))
        .map(|v| content(v, decor.horizontal()))
        .unwrap_or(0.0);
    let max = length(style, MaxWidth, Some(width))
        .map(|v| content(v, decor.horizontal()))
        .unwrap_or(f64::INFINITY);
    used_width = used_width.min(max).max(min);
    let min_height = length(style, MinHeight, height)
        .map(|v| content(v, decor.vertical()))
        .unwrap_or(0.0);
    let max_height = length(style, MaxHeight, height)
        .map(|v| content(v, decor.vertical()))
        .unwrap_or(f64::INFINITY);
    if let Some(h) = used_height {
        used_height = Some(h.min(max_height).max(min_height));
    }
    let block = b.kind == BoxKind::Block
        || (b.kind == BoxKind::Image
            && !matches!(keyword(style, Display), "inline" | "inline-block"));
    if block {
        let free = width - used_width - decor.horizontal() - margin.horizontal();
        let auto_left = length(style, MarginLeft, Some(width)).is_none();
        let auto_right = length(style, MarginRight, Some(width)).is_none();
        if free > 0.0 && (auto_left || auto_right) {
            if auto_left {
                margin.left = if auto_right { free / 2.0 } else { free };
            }
            if auto_right {
                margin.right = if auto_left { free / 2.0 } else { free };
            }
        } else {
            margin.right += free;
        }
    }
    if !coordinate(used_width) || used_height.is_some_and(|h| !coordinate(h)) {
        return Err(Error::Limit("MAX_COORDINATE"));
    }
    let relative = if keyword(style, Position) == "relative" {
        (
            length(style, Left, Some(width))
                .or_else(|| length(style, Right, Some(width)).map(|v| -v))
                .unwrap_or(0.0),
            length(style, Top, height)
                .or_else(|| length(style, Bottom, height).map(|v| -v))
                .unwrap_or(0.0),
        )
    } else {
        (0.0, 0.0)
    };
    Ok(Dimensions {
        width: used_width,
        height: used_height,
        margin,
        padding,
        border,
        min_height,
        max_height,
        relative,
    })
}
fn collapse(a: f64, b: f64) -> f64 {
    a.max(b).max(0.0) + a.min(b).min(0.0)
}
fn alignment(align: &str, free: f64) -> f64 {
    match align {
        "right" | "end" => free,
        "center" => free / 2.0,
        _ => 0.0,
    }
}

fn include_bounds(
    left: &mut f64,
    top: &mut f64,
    right: &mut f64,
    bottom: &mut f64,
    rect: Rect,
) -> Result<(), Error> {
    for n in [
        rect.x,
        rect.y,
        rect.width,
        rect.height,
        rect.x + rect.width,
        rect.y + rect.height,
    ] {
        if !coordinate(n) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
    }
    *left = left.min((rect.x * 64.0).floor() / 64.0);
    *top = top.min((rect.y * 64.0).floor() / 64.0);
    *right = right.max(((rect.x + rect.width) * 64.0).ceil() / 64.0);
    *bottom = bottom.max(((rect.y + rect.height) * 64.0).ceil() / 64.0);
    if !coordinate(*right - *left) || !coordinate(*bottom - *top) {
        return Err(Error::Limit("MAX_COORDINATE"));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::inline::FontLine;
    use crate::Limits;
    use td_css::{Origin, StyleSet, HTML_UA_CSS};
    struct Metrics;
    impl FontMetrics for Metrics {
        fn advance(&mut self, _: &ComputedStyle, _: char) -> u32 {
            8 * 64
        }
        fn line(&mut self, s: &ComputedStyle) -> FontLine {
            FontLine {
                height: (s.font_size() * 80.0) as u32,
                baseline: (s.font_size() * 60.0) as u32,
            }
        }
    }
    fn sheets(css: &str) -> StyleSet {
        StyleSet::new(&[
            (Origin::UserAgent, HTML_UA_CSS),
            (Origin::Author, "*{margin:0;padding:0;border-width:0}"),
            (Origin::Author, css),
        ])
        .unwrap()
    }
    fn find<'a>(doc: &td_html::document::Document, out: &'a Layout, id: &str) -> &'a BoxGeometry {
        out.boxes
            .iter()
            .find(|g| {
                doc.node(g.node)
                    .and_then(|n| n.element())
                    .and_then(|e| e.attribute("id"))
                    == Some(id)
            })
            .unwrap()
    }
    #[test]
    fn widths_box_sizing_and_percentage_edges() {
        let html = "<div id=outer><p id=inner>x</p></div>";
        let doc = td_html::document::parse(html).unwrap();
        let css=sheets("#outer{width:200px;padding:10px;border:2px solid;margin:auto}#inner{width:50%;padding:10%}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(400.0, 300.0)).unwrap();
        let outer = find(&doc, &out, "outer");
        let inner = find(&doc, &out, "inner");
        assert_eq!(outer.content.x, 100.0);
        assert_eq!(outer.border.width, 224.0);
        assert_eq!(outer.margin.left, 88.0);
        assert_eq!(outer.margin.right, 88.0);
        assert_eq!(inner.content.width, 100.0);
        assert_eq!(inner.border.width, 140.0);
        assert_eq!(inner.content.x, 120.0);
        let css = sheets("#outer{width:200px;padding:10px;border:2px solid;box-sizing:border-box}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(400.0, 300.0)).unwrap();
        assert_eq!(find(&doc, &out, "outer").content.width, 176.0);
    }
    #[test]
    fn sibling_margin_collapse_and_relative_flow_space() {
        let html = "<div id=a></div> \n <div id=b></div>";
        let doc = td_html::document::parse(html).unwrap();
        for (bottom, top, want) in [(20, 30, 40.0), (20, -5, 25.0), (-20, -30, -20.0)] {
            let css = sheets(&format!(
                "div{{height:10px}}#a{{margin-bottom:{bottom}px}}#b{{margin-top:{top}px}}"
            ));
            let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).unwrap();
            assert_eq!(find(&doc, &out, "b").border.y, want);
        }
        let css = sheets("div{height:20px}#a{position:relative;left:5px;top:10px}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).unwrap();
        assert_eq!(find(&doc, &out, "a").border.y, 10.0);
        assert_eq!(find(&doc, &out, "a").border.x, 5.0);
        assert_eq!(find(&doc, &out, "b").border.y, 20.0);
    }
    #[test]
    fn definite_height_percentages_and_min_max() {
        let html = "<div id=outer><div id=inner>x</div></div>";
        let doc = td_html::document::parse(html).unwrap();
        for (outer_height, want) in [("200px", 100.0), ("auto", 20.0)] {
            let css=sheets(&format!("#outer{{height:{outer_height}}}#inner{{height:50%;width:90%;max-width:60px;min-width:40px}}"));
            let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(100.0, 300.0)).unwrap();
            let inner = find(&doc, &out, "inner");
            assert_eq!(inner.content.height, want);
            assert_eq!(inner.content.width, 60.0);
        }
        let css = sheets(
            "#outer{width:200px;height:200px}#inner{width:50px;position:relative;left:50%;top:50%}",
        );
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(300.0, 300.0)).unwrap();
        assert_eq!(find(&doc, &out, "inner").content.x, 100.0);
        assert_eq!(find(&doc, &out, "inner").content.y, 100.0);
    }
    #[test]
    fn inline_images_have_attribute_placeholders_and_source_ids() {
        for (attributes, css, w, h) in [
            ("", "", 300.0, 150.0),
            ("width=40 height=20", "", 40.0, 20.0),
            ("width=40 height=20", "img{width:80px}", 80.0, 40.0),
            ("width=40", "", 40.0, 150.0),
            ("width=-2 height=nan", "", 300.0, 150.0),
        ] {
            let html = format!("<p>a<img {attributes}>b");
            let doc = td_html::document::parse(&html).unwrap();
            let styled =
                StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(500.0, 300.0)).unwrap();
            assert_eq!(out.lines.len(), 1);
            let image = &out.lines[0].replaced[0];
            assert_eq!((image.width, image.height), (w, h));
            assert_eq!(image.x, 8.0);
            assert_eq!(doc.node(image.node).unwrap().element().unwrap().name, "img");
        }
    }
    #[test]
    fn float_fallback_inline_block_splits_and_alignment() {
        let html = "<div id=float></div><span>before<div id=block>x</div>after</span>";
        let doc = td_html::document::parse(html).unwrap();
        let css = sheets(
            "#float{float:left;width:30px;height:20px}#block{height:10px}body{text-align:center}",
        );
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(100.0, 200.0)).unwrap();
        assert_eq!(out.lines.len(), 3);
        assert_eq!(out.lines[0].y, 20.0);
        assert_eq!(out.lines[0].fragments[0].x, 26.0);
        assert_eq!(find(&doc, &out, "block").border.y, 40.0);
        assert_eq!(out.lines[2].y, 50.0);
    }
    #[test]
    fn windowed_output_equals_filtering_full_and_resize_charges_overlap() {
        let html = format!("<p>{}", "hello world ".repeat(500));
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets("");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let request = Request::new(100.0, 200.0);
        let full = layout(&styled, &mut Metrics, &request).unwrap();
        let mut window = request.clone();
        window.window = Some(100.0..200.0);
        let subset = layout(&styled, &mut Metrics, &window).unwrap();
        assert_eq!(
            subset.lines,
            full.lines
                .iter()
                .filter(|l| l.y + l.height > 100.0 && l.y < 200.0)
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(subset.height, full.height);
        assert_eq!(subset.boxes, full.boxes);
        assert_eq!(subset.line_count, full.line_count);
        assert!(subset.output_bytes < full.output_bytes);
        window.width = 80.0;
        window.retained_output_bytes = full.output_bytes;
        let resized = layout(&styled, &mut Metrics, &window).unwrap();
        assert_eq!(
            resized.charged_bytes,
            styled.charged_bytes() + full.output_bytes + resized.output_bytes
        );
        assert!(resized.line_count > full.line_count);
        window.retained_output_bytes = crate::MAX_PAGE_BYTES;
        assert!(matches!(
            layout(&styled, &mut Metrics, &window),
            Err(Error::Limit("MAX_PAGE_BYTES"))
        ));
    }
    #[test]
    fn viewport_and_coordinate_admission_edges() {
        let doc = td_html::document::parse("<p>x").unwrap();
        let styled = StyledDocument::new(&doc, &sheets(""), 4, Limits::default()).unwrap();
        assert!(layout(
            &styled,
            &mut Metrics,
            &Request::new(MAX_VIEWPORT, MAX_VIEWPORT)
        )
        .is_ok());
        for width in [MAX_VIEWPORT + 1.0, -1.0, f64::INFINITY, f64::NAN] {
            assert!(matches!(
                layout(&styled, &mut Metrics, &Request::new(width, 100.0)),
                Err(Error::Limit("MAX_VIEWPORT"))
            ));
        }
        let mut r = Request::new(100.0, 100.0);
        r.window = Some(10.0..0.0);
        assert!(matches!(
            layout(&styled, &mut Metrics, &r),
            Err(Error::Invalid)
        ));
    }
    #[test]
    fn explicit_breaks_and_images_separate_crlf_and_use_break_metrics() {
        for (html, want_lines) in [
            ("<p>a&#13;<br>&#10;b", 4),
            ("<p>a&#13;<img width=1 height=1>&#10;b", 3),
        ] {
            let doc = td_html::document::parse(html).unwrap();
            let styled = StyledDocument::new(
                &doc,
                &sheets("p{white-space:pre-wrap}"),
                html.len(),
                Limits::default(),
            )
            .unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(300.0, 300.0)).unwrap();
            assert_eq!(out.lines.len(), want_lines, "{html}");
        }
        let html = "<p>x<br style='font-size:100px'>y";
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&doc, &sheets(""), html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(300.0, 300.0)).unwrap();
        assert_eq!(out.lines[0].height, 125.0);
        assert_eq!(out.lines[1].y, 125.0);
        assert_eq!(out.lines[1].height, 20.0);
    }
    #[test]
    fn intrinsic_and_constrained_blocks_center_auto_margins() {
        for (html, css, id, width) in [
            (
                "<img id=x width=40 height=20>",
                "img{display:block;margin:auto}",
                "x",
                40.0,
            ),
            (
                "<div id=x>x</div>",
                "div{max-width:40px;margin:auto}",
                "x",
                40.0,
            ),
        ] {
            let doc = td_html::document::parse(html).unwrap();
            let styled =
                StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).unwrap();
            let b = find(&doc, &out, id);
            assert_eq!(b.border.width, width);
            assert_eq!(b.border.x, 30.0);
        }
    }

    #[test]
    fn overflowing_alignment_and_image_margins_reach_overflow_bounds() {
        let html = "<p><img width=200 height=20>";
        let doc = td_html::document::parse(html).unwrap();
        for (align, x) in [("center", -50.0), ("right", -100.0)] {
            let css = sheets(&format!("p{{text-align:{align}}}"));
            let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).unwrap();
            assert_eq!(out.lines[0].replaced[0].x, x);
            assert_eq!(out.overflow.x, x);
        }
        let styled = StyledDocument::new(
            &doc,
            &sheets("img{margin-left:-20px;margin-top:-30px;margin-bottom:-40px}"),
            html.len(),
            Limits::default(),
        )
        .unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(300.0, 100.0)).unwrap();
        let image = &out.lines[0].replaced[0];
        assert!(out.overflow.x <= image.x);
        assert!(out.overflow.y <= image.y);
        assert!(out.overflow.y + out.overflow.height >= image.y + image.height);
        let mut request = Request::new(300.0, 100.0);
        request.window = Some(0.0..0.0);
        assert!(layout(&styled, &mut Metrics, &request)
            .unwrap()
            .lines
            .is_empty());
    }

    #[test]
    fn aggregate_overflow_extent_has_an_exact_limit() {
        let html = "<img width=1000000 height=1><div>x</div>";
        let doc = td_html::document::parse(html).unwrap();
        for (left, ok) in [(0, true), (-1, false)] {
            let css=sheets(&format!("img{{display:block;height:1000px}}div{{width:1px;position:relative;left:{left}px}}"));
            let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
            assert_eq!(
                layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).is_ok(),
                ok
            );
        }
    }
    #[test]
    fn wrapping_image_drops_all_space_prefix() {
        let html = "<p>    <img width=8 height=10>";
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(
            &doc,
            &sheets("p{white-space:pre-wrap}"),
            html.len(),
            Limits::default(),
        )
        .unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(16.0, 100.0)).unwrap();
        assert_eq!(out.lines.len(), 1);
        assert_eq!(out.lines[0].y, 0.0);
    }
    #[test]
    fn image_outer_advance_keeps_negative_margins() {
        for (prefix, margin, want) in [("aaaa", -30, 22.0), ("a", -100, -72.0)] {
            let html = format!("<p>{prefix}<img width=20 height=20>b");
            let doc = td_html::document::parse(&html).unwrap();
            let styled = StyledDocument::new(
                &doc,
                &sheets(&format!("img{{margin-left:{margin}px}}")),
                html.len(),
                Limits::default(),
            )
            .unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).unwrap();
            let last = out.lines[0].fragments.last().unwrap();
            assert_eq!(last.x, want);
            assert!(out.overflow.x <= out.lines[0].replaced[0].x);
        }
    }
    #[test]
    fn fractional_image_geometry_stays_inside_overflow() {
        let html = "<p><img width=20 height=20.001>";
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&doc, &sheets(""), html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).unwrap();
        let image = &out.lines[0].replaced[0];
        assert!(out.overflow.y <= image.y);
        assert!(out.overflow.y + out.overflow.height >= image.y + image.height);
    }
    #[test]
    fn final_margin_is_in_the_coordinate_admission() {
        let html = "<img width=1 height=1000000>";
        let doc = td_html::document::parse(html).unwrap();
        for (margin, ok) in [(0, true), (1, false)] {
            let styled = StyledDocument::new(
                &doc,
                &sheets(&format!(
                    "img{{display:block;width:1000px}}html{{margin-bottom:{margin}px}}"
                )),
                html.len(),
                Limits::default(),
            )
            .unwrap();
            assert_eq!(
                layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).is_ok(),
                ok
            );
        }
    }

    #[test]
    fn signed_fractional_inline_geometry_and_windows_agree() {
        let html = "<p>aaaa <img width=20.001 height=20.001> b cdef ghi";
        let doc = td_html::document::parse(html).unwrap();
        for left in [-40, -5, 0, 5] {
            for right in [-40, 0, 5] {
                for align in ["left", "center", "right"] {
                    let css = sheets(&format!(
                        "p{{text-align:{align}}}img{{margin:{left}px {right}px}}"
                    ));
                    let styled =
                        StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
                    for width in [8.0, 16.0, 32.0, 80.0] {
                        let mut request = Request::new(width, 100.0);
                        let full = layout(&styled, &mut Metrics, &request).unwrap();
                        for line in &full.lines {
                            for image in &line.replaced {
                                assert!(image.x >= full.overflow.x);
                                assert!(image.y >= full.overflow.y);
                                assert!(
                                    image.x + image.width <= full.overflow.x + full.overflow.width
                                );
                                assert!(
                                    image.y + image.height
                                        <= full.overflow.y + full.overflow.height
                                );
                            }
                            for text in &line.fragments {
                                assert!(text.x >= full.overflow.x);
                                assert!(
                                    text.x + text.width <= full.overflow.x + full.overflow.width
                                );
                            }
                        }
                        request.window = Some(20.0..40.0);
                        let window = layout(&styled, &mut Metrics, &request).unwrap();
                        assert_eq!(full.overflow, window.overflow);
                        assert_eq!(
                            window.lines,
                            full.lines
                                .iter()
                                .filter(|l| l.y + l.height > 20.0 && l.y < 40.0)
                                .cloned()
                                .collect::<Vec<_>>()
                        );
                    }
                }
            }
        }
    }
}
