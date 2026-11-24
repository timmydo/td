//! Min-content and max-content widths (CSS 2.2 §17.5.2.2's minimum and
//! maximum cell widths) of block content, for table columns: the widest
//! piece nothing may break and the widest line only forced breaks end,
//! by the same white space, wrap opportunities and advances inline layout
//! uses. Work is charged per box and scalar, as layout's is.
use super::table::{units, up, Measured};
use super::*;
use crate::inline::{advance, letter_spacing, shown, tab_advance, tab_interval, white_space};

/// Work a measured block, table or image costs: its style read, its
/// edges resolved and its children gathered or size taken take some ten
/// times a scalar's time, which a unit of work is (DESIGN.md §6).
pub(super) const MEASURE_BOX: usize = 8;
/// Whole px, rounded half up as a line's fit test rounds, in 1/64 px.
fn whole(units: i64) -> i64 {
    units.saturating_add(32).div_euclid(64).saturating_mul(64)
}
/// An inline sequence measured so far.
#[derive(Default)]
struct Measure {
    /// The line's pen, its last non-space's end.
    line: i64,
    /// Spaces after it, laid only if a non-space follows on the line.
    pending: i64,
    /// The piece no opportunity breaks, and spaces inside it pending.
    word: i64,
    word_pending: i64,
    min: i64,
    max: i64,
    /// A collapsible space was the last scalar, or the line just started.
    last_space: bool,
    previous_cr: bool,
}
impl Measure {
    fn new() -> Self {
        Self {
            last_space: true,
            ..Self::default()
        }
    }
    fn opportunity(&mut self) {
        self.min = self.min.max(whole(self.word));
        self.word = 0;
        self.word_pending = 0;
    }
    fn solid(&mut self, advance: i64) {
        self.line = self
            .line
            .saturating_add(self.pending)
            .saturating_add(advance);
        self.pending = 0;
        self.word = self
            .word
            .saturating_add(self.word_pending)
            .saturating_add(advance);
        self.word_pending = 0;
    }
    fn space(&mut self, advance: i64, soft: bool) {
        self.pending = self.pending.saturating_add(advance);
        if soft {
            self.opportunity();
        } else {
            self.word_pending = self.word_pending.saturating_add(advance);
        }
    }
    fn hard_break(&mut self) {
        self.opportunity();
        self.max = self.max.max(whole(self.line));
        self.line = 0;
        self.pending = 0;
        self.last_space = true;
    }
}

impl<M: FontMetrics> Engine<'_, '_, '_, '_, M> {
    /// The intrinsic widths of the content of a block container, the
    /// siblings from `first` up to `until`, in whole px as 1/64 px.
    pub(super) fn content_intrinsic(
        &mut self,
        first: Option<BoxId>,
        until: Option<BoxId>,
        strut: StyleId,
        depth: usize,
    ) -> Result<(i64, i64), Error> {
        self.content_with(first, until, strut, None, depth)
    }
    /// As content_intrinsic, led by an inside marker's advance and whether
    /// a line may wrap after it. The events' storage is charged while it is
    /// held and given back after, so that measuring the same content for
    /// each table around it charges its peak, not a multiple.
    pub(super) fn content_with(
        &mut self,
        first: Option<BoxId>,
        until: Option<BoxId>,
        strut: StyleId,
        mut lead: Option<(i64, bool)>,
        depth: usize,
    ) -> Result<(i64, i64), Error> {
        if depth > self.counters.limits.depth {
            return Err(Error::Limit("MAX_DEPTH"));
        }
        let before = self.counters.bytes;
        let events = self.events(first, until)?;
        let held = self.counters.bytes - before;
        let measured = self.measure_events(&events, strut, &mut lead, depth);
        self.counters.bytes -= held;
        let (mut min, mut max) = measured?;
        // A marker before nothing inline is a line of its own.
        if let Some((advance, _)) = lead {
            min = min.max(whole(advance));
            max = max.max(whole(advance));
        }
        Ok((min, max.max(min)))
    }
    fn measure_events(
        &mut self,
        events: &Events,
        strut: StyleId,
        lead: &mut Option<(i64, bool)>,
        depth: usize,
    ) -> Result<(i64, i64), Error> {
        let (mut min, mut max) = (0i64, 0i64);
        let mut at = 0;
        while let Some(event) = events.get(at).copied() {
            let (least, most) = match event {
                Event::Block(id) => {
                    if let Some((advance, _)) = lead.take() {
                        min = min.max(whole(advance));
                        max = max.max(whole(advance));
                    }
                    let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
                    let role = self.role(&b)?;
                    if let Some(end) = self.table_run(events, at, role)? {
                        let measured = self.table_intrinsic(
                            Measured::Anonymous(events, at..end),
                            strut,
                            depth + 1,
                        )?;
                        at = end;
                        measured
                    } else {
                        at += 1;
                        self.block_intrinsic(id, b, role, depth + 1)?
                    }
                }
                Event::Inline(_) | Event::Float(_) => {
                    let start = at;
                    while events.get(at).is_some_and(|e| e.flows()) {
                        at += 1;
                    }
                    self.inline_intrinsic(events, start..at, strut, lead.take(), depth)?
                }
            };
            min = min.max(least);
            max = max.max(most);
        }
        Ok((min, max))
    }
    /// A list item's inside marker, measured as layout lays it: its
    /// advance, and whether its item's lines wrap after it.
    pub(super) fn inside_marker(
        &mut self,
        id: BoxId,
        b: &BoxNode,
    ) -> Result<Option<(i64, bool)>, Error> {
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        if b.kind != BoxKind::Block
            || keyword(style, Property::Display) != "list-item"
            || keyword(style, Property::ListStylePosition) != "inside"
        {
            return Ok(None);
        }
        let Some(kind) = MarkerKind::from_keyword(keyword(style, Property::ListStyleType)) else {
            return Ok(None);
        };
        let soft = white_space(style).2;
        let ordinal = self.styled.ordinal(id).ok_or(Error::Invalid)?;
        let marker = Marker::new(
            self.styled,
            self.metrics,
            (b.node, b.style),
            (kind, ordinal),
            None,
            &mut self.counters,
        )?;
        Ok(Some((marker.advance, soft)))
    }
    /// A block-level box's contribution: its intrinsic content widths, or
    /// its width where set in px, within its px min and max, with its
    /// padding, border and margins; percentages are auto, or 0 for edges.
    pub(super) fn block_intrinsic(
        &mut self,
        id: BoxId,
        b: BoxNode,
        role: Option<TableRole>,
        depth: usize,
    ) -> Result<(i64, i64), Error> {
        self.counters.step(MEASURE_BOX)?;
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        let margin = edges(
            style,
            [
                Property::MarginTop,
                Property::MarginRight,
                Property::MarginBottom,
                Property::MarginLeft,
            ],
            0.0,
            false,
        );
        let around = up(margin.horizontal());
        if b.kind == BoxKind::Image {
            let (least, most) = self.image_intrinsic(&b)?;
            return Ok(((least + around).max(0), (most + around).max(0)));
        }
        let (least, most) = if role == Some(TableRole::Table) {
            self.table_intrinsic(Measured::Root(id), b.style, depth)?
        } else {
            let dims = sized(self.styled, &b, style, None, None, None)?;
            let decoration = dims.decoration().horizontal();
            let to_content = |v: f64| {
                if keyword(style, Property::BoxSizing) == "border-box" {
                    (v - decoration).max(0.0)
                } else {
                    v.max(0.0)
                }
            };
            let low = length(style, Property::MinWidth, None).map_or(0.0, to_content);
            let high = length(style, Property::MaxWidth, None).map_or(f64::INFINITY, to_content);
            let bound = |w: i64| units((w as f64 / 64.0).min(high).max(low));
            let (least, most) = match length(style, Property::Width, None) {
                Some(width) => {
                    let w = bound(up(to_content(width)));
                    (w, w)
                }
                None => {
                    let lead = self.inside_marker(id, &b)?;
                    let (least, most) =
                        self.content_with(b.first_child, None, b.style, lead, depth)?;
                    (bound(least), bound(most))
                }
            };
            (least + up(decoration), most + up(decoration))
        };
        Ok(((least + around).max(0), (most + around).max(0)))
    }
    /// A replaced element's border box: its size with percentages as auto,
    /// and none at all where its width or maximum is a percentage, which
    /// may shrink it to nothing (CSS Sizing 3 §5.2.2).
    fn image_intrinsic(&mut self, b: &BoxNode) -> Result<(i64, i64), Error> {
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        let dims = sized(self.styled, b, style, None, None, lookup(self.request, b))?;
        let decoration = up(dims.decoration().horizontal());
        let most = units(dims.width) + decoration;
        let relative = |p| {
            matches!(
                style.get(p),
                Some(ComputedValue::Length(
                    Length::Percent(_) | Length::Calc { .. }
                ))
            )
        };
        let least = if relative(Property::Width) || relative(Property::MaxWidth) {
            decoration
        } else {
            most
        };
        Ok((least, most))
    }
    /// An inline sequence: its widest unbreakable piece and its widest
    /// line between forced breaks, as Flow lays them (DESIGN.md §4).
    fn inline_intrinsic(
        &mut self,
        events: &Events,
        range: Range<usize>,
        strut: StyleId,
        lead: Option<(i64, bool)>,
        depth: usize,
    ) -> Result<(i64, i64), Error> {
        let mut m = Measure::new();
        let mut stops: Option<i64> = None;
        // An inside marker: whole, a wrap opportunity after it where its
        // item wraps, and spaces after it dropped as at a line's start.
        if let Some((advance, soft)) = lead {
            self.counters.step(1)?;
            m.solid(advance);
            if soft {
                m.opportunity();
            }
        }
        for index in range {
            let id = match events.get(index).copied() {
                Some(Event::Inline(id)) => id,
                // A float: no wrap opportunity, as laid; its own least a
                // piece of its own, and its widest beside its line's
                // and the space before it, which the line's pen holds
                // when it is placed (DESIGN.md §5, "Floats").
                Some(Event::Float(id)) => {
                    let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
                    let (least, most) = self.block_intrinsic(id, b, None, depth + 1)?;
                    m.min = m.min.max(whole(least));
                    m.line = m.line.saturating_add(m.pending).saturating_add(most);
                    m.pending = 0;
                    continue;
                }
                _ => return Err(Error::Invalid),
            };
            let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
            let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
            match b.kind {
                BoxKind::Text => {
                    let text = self
                        .styled
                        .document()
                        .node(b.node)
                        .and_then(|n| n.text())
                        .ok_or(Error::Invalid)?;
                    let (preserve, newline, soft) = white_space(style);
                    let spacing = letter_spacing(style);
                    for scalar in text.chars() {
                        self.counters.step(1)?;
                        if m.previous_cr && scalar == '\n' && newline {
                            m.previous_cr = false;
                            continue;
                        }
                        m.previous_cr = scalar == '\r' && newline;
                        if newline && matches!(scalar, '\n' | '\r') {
                            m.hard_break();
                            m.previous_cr = scalar == '\r';
                            continue;
                        }
                        let ascii_space = matches!(scalar, ' ' | '\t' | '\n' | '\r' | '\x0c');
                        if !preserve && ascii_space && m.last_space {
                            continue;
                        }
                        m.last_space = !preserve && ascii_space;
                        let (shown, _) = shown(scalar, preserve);
                        if shown == '\t' {
                            let interval = match stops {
                                Some(interval) => interval,
                                None => {
                                    let block = self.styled.style(strut).ok_or(Error::Invalid)?;
                                    let interval =
                                        tab_interval(self.metrics, block, letter_spacing(block))?;
                                    stops = Some(interval);
                                    interval
                                }
                            };
                            let pen = m.line.saturating_add(m.pending);
                            m.space(tab_advance(pen, interval), soft);
                        } else if shown == ' ' {
                            m.space(advance(self.metrics, style, ' ', spacing), soft);
                        } else {
                            m.solid(advance(self.metrics, style, shown, spacing));
                        }
                    }
                }
                BoxKind::Break => {
                    self.counters.step(1)?;
                    m.hard_break();
                }
                BoxKind::Image => {
                    self.counters.step(MEASURE_BOX)?;
                    let margin = edges(
                        style,
                        [
                            Property::MarginTop,
                            Property::MarginRight,
                            Property::MarginBottom,
                            Property::MarginLeft,
                        ],
                        0.0,
                        false,
                    );
                    let (least, most) = self.image_intrinsic(&b)?;
                    let around = up(margin.horizontal());
                    let soft = !matches!(keyword(style, Property::WhiteSpace), "pre" | "nowrap");
                    if soft {
                        // A wrap opportunity on each side (DESIGN.md §5).
                        m.opportunity();
                        m.min = m.min.max(whole(least + around));
                        m.line = m
                            .line
                            .saturating_add(m.pending)
                            .saturating_add(most + around);
                        m.pending = 0;
                    } else {
                        m.solid(most + around);
                    }
                    m.last_space = false;
                    m.previous_cr = false;
                }
                _ => return Err(Error::Invalid),
            }
        }
        m.hard_break();
        Ok((m.min.max(0), m.max.max(m.min).max(0)))
    }
}
