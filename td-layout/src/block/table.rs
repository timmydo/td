//! CSS 2.2 §17 tables: anonymous table objects (§17.2.1), HTML's grid of
//! `colspan` and `rowspan`, the automatic and fixed width algorithms
//! (§17.5.2), separated borders with `collapse` approximated (§17.6),
//! cell vertical alignment (§17.5.3) and captions. See DESIGN.md §5,
//! "Tables".
use super::*;
use crate::{MAX_COLSPAN, MAX_COORDINATE, MAX_ROWSPAN, MAX_TABLE_COLUMNS};

/// What a table is laid from: a table box, an inline table's, or a run of
/// table boxes with no table in a block's events, which an anonymous
/// table wraps.
pub(super) enum Source<'e> {
    Root(BoxId, Dimensions),
    Inline(BoxId, Dimensions),
    Anonymous(&'e Events, Range<usize>),
}
/// A table measured for its intrinsic widths: a table box, or a run of
/// table boxes with no table.
pub(super) enum Measured<'e> {
    Root(BoxId),
    Anonymous(&'e Events, Range<usize>),
}
/// A cell's content: a cell box's children, or a run of siblings no cell
/// holds, which an anonymous cell wraps, up to `until`.
#[derive(Clone, Copy)]
pub(super) enum Content {
    Cell(BoxId),
    Run(BoxId, Option<BoxId>),
}
/// A cell placed in its table's grid. Rows and columns count from the
/// table's first; `min` and `max` are its border box's intrinsic widths
/// in 1/64 px.
#[derive(Clone, Copy)]
pub(super) struct Cell {
    content: Content,
    row: u32,
    col: u32,
    colspan: u32,
    /// HTML's, then cut at its row group's end; 0 spans to that end.
    rowspan: u32,
    min: i64,
    max: i64,
    percent: f64,
    fixed: bool,
}
#[derive(Clone, Copy)]
pub(super) struct Row {
    node: Option<BoxId>,
    /// The style an anonymous cell in it is laid with: its own, else its
    /// group's or its table's.
    strut: StyleId,
    cells: usize,
    end: usize,
    y: f64,
    height: f64,
}
/// A column of the grid, then of the effective grid: a cell edge or a
/// column element's starts each.
#[derive(Clone, Copy, Default)]
pub(super) struct Col {
    min: i64,
    max: i64,
    percent: f64,
    fixed: bool,
    width: i64,
    x: i64,
    /// While the grid is built: the row group whose rowspans hold it, the
    /// row they hold it until, whether an edge starts here, and its
    /// effective column.
    group: u32,
    until: u32,
    edge: bool,
    effective: u32,
}
/// The table's children in tree order, as they are laid.
#[derive(Clone, Copy)]
pub(super) enum Item {
    Caption(BoxId),
    /// A column or column group box: its first column, its span, whether
    /// its own width applies (a column, or a group without columns) and
    /// its geometry's index once laid.
    Column {
        node: BoxId,
        first: u32,
        span: u32,
        own: bool,
        at: usize,
    },
    Row(u32),
    Group {
        node: BoxId,
        first: u32,
        end: u32,
    },
}
/// A cell laid in its row, its content not yet aligned.
#[derive(Clone, Copy)]
pub(super) struct Laid {
    top: u32,
    last: u32,
    lines: (usize, usize),
    boxes: (usize, usize),
    markers: (usize, usize),
    geometry: Option<usize>,
    /// The border-box height it needs, its set height included, and its
    /// content's alone, which it is aligned by.
    height: f64,
    content: f64,
    /// Its first line's baseline from its border top.
    baseline: f64,
    align: Align,
    bounds: Option<Rect>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Top,
    Middle,
    Bottom,
    Baseline,
}
/// Every table's grid being laid or measured, a nested table's after its
/// ancestors'; storage is reused, charged as it grows.
pub(super) struct Scratch {
    cells: BudgetVec<Cell>,
    rows: BudgetVec<Row>,
    cols: BudgetVec<Col>,
    items: BudgetVec<Item>,
    laid: BudgetVec<Laid>,
    /// Spanning cells by span, while a table is measured.
    order: BudgetVec<(u32, usize)>,
}
/// Where one table's records start.
#[derive(Clone, Copy)]
struct Grid {
    cells: usize,
    rows: usize,
    cols: usize,
    items: usize,
    laid: usize,
}
impl Scratch {
    pub(super) fn new(memory: &Budget) -> Result<Self, Error> {
        Ok(Self {
            cells: BudgetVec::new(memory)?,
            rows: BudgetVec::new(memory)?,
            cols: BudgetVec::new(memory)?,
            items: BudgetVec::new(memory)?,
            laid: BudgetVec::new(memory)?,
            order: BudgetVec::new(memory)?,
        })
    }
    fn base(&self) -> Grid {
        Grid {
            cells: self.cells.len(),
            rows: self.rows.len(),
            cols: self.cols.len(),
            items: self.items.len(),
            laid: self.laid.len(),
        }
    }
    fn truncate(&mut self, grid: Grid) {
        self.cells.truncate(grid.cells);
        self.rows.truncate(grid.rows);
        self.cols.truncate(grid.cols);
        self.items.truncate(grid.items);
        self.laid.truncate(grid.laid);
    }
}
/// A table's own box and the values its grid is laid with.
struct Frame {
    node: Option<(BoxId, StyleId)>,
    style: StyleId,
    collapse: bool,
    /// border-spacing in 1/64 px: zero where borders collapse.
    across: i64,
    down: f64,
    padding: Edges,
    border: Edges,
    /// Content width and height set by CSS, with their bounds.
    width: Option<f64>,
    min_width: f64,
    max_width: f64,
    height: Option<f64>,
    min_height: f64,
    max_height: f64,
    fixed: bool,
}
impl Frame {
    fn decoration(&self) -> Edges {
        self.padding.plus(self.border)
    }
}
/// Children of a table, a row group or a row, one ahead.
struct Walk<'e> {
    kids: Kids<'e>,
    peeked: Option<BoxId>,
}
enum Kids<'e> {
    Chain {
        next: Option<BoxId>,
        until: Option<BoxId>,
    },
    Events {
        events: &'e Events,
        at: usize,
        end: usize,
    },
}
impl<'e> Walk<'e> {
    fn chain(first: Option<BoxId>, until: Option<BoxId>) -> Self {
        Self {
            kids: Kids::Chain { next: first, until },
            peeked: None,
        }
    }
    fn events(events: &'e Events, range: Range<usize>) -> Self {
        Self {
            kids: Kids::Events {
                events,
                at: range.start,
                end: range.end,
            },
            peeked: None,
        }
    }
    fn peek(&mut self, styled: &StyledDocument<'_>) -> Result<Option<BoxId>, Error> {
        if self.peeked.is_none() {
            self.peeked = match &mut self.kids {
                Kids::Chain { next, until } => match next.filter(|id| Some(*id) != *until) {
                    Some(id) => {
                        *next = styled.box_node(id).ok_or(Error::Invalid)?.next_sibling;
                        Some(id)
                    }
                    None => None,
                },
                Kids::Events { events, at, end } => {
                    if *at < *end {
                        let event = events.get(*at).ok_or(Error::Invalid)?;
                        *at += 1;
                        Some(match event {
                            Event::Inline(id)
                            | Event::Block(id)
                            | Event::Float(id)
                            | Event::Atomic(id) => *id,
                        })
                    } else {
                        None
                    }
                }
            };
        }
        Ok(self.peeked)
    }
    fn take(&mut self) {
        self.peeked = None;
    }
    /// Whether consecutive children are siblings, so a run of them is a
    /// sibling range.
    fn chained(&self) -> bool {
        matches!(self.kids, Kids::Chain { .. })
    }
}
/// A child's part where a table, row group or row holds it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Text of white space alone, dropped between table boxes.
    Blank,
    Other,
    Proper(TableRole),
}
/// Where an anonymous row or cell gathering children stops.
#[derive(Clone, Copy)]
enum Stop {
    Table,
    Group,
    Never,
}
impl Stop {
    fn at(self, kind: Kind) -> bool {
        use TableRole::*;
        matches!(
            (self, kind),
            (
                Self::Table,
                Kind::Proper(Caption | ColumnGroup | Column | RowGroup | Row)
            ) | (Self::Group, Kind::Proper(Row))
        )
    }
}
/// 1/64 px, nearest.
pub(super) fn units(px: f64) -> i64 {
    (px * 64.0)
        .round()
        .clamp(-MAX_COORDINATE * 64.0, MAX_COORDINATE * 64.0) as i64
}
/// 1/64 px, rounded up: an edge an intrinsic width adds, so content laid
/// at that width less the exact edge is never narrower than measured.
pub(super) fn up(px: f64) -> i64 {
    (px * 64.0)
        .ceil()
        .clamp(-MAX_COORDINATE * 64.0, MAX_COORDINATE * 64.0) as i64
}
/// 1/64 px, rounded down: a width taken from the room a container
/// leaves, so the columns never overflow it.
fn down(px: f64) -> i64 {
    (px * 64.0)
        .floor()
        .clamp(-MAX_COORDINATE * 64.0, MAX_COORDINATE * 64.0) as i64
}
fn px(units: i64) -> f64 {
    units as f64 / 64.0
}
const PADDING: [Property; 4] = [
    Property::PaddingTop,
    Property::PaddingRight,
    Property::PaddingBottom,
    Property::PaddingLeft,
];
const BORDER: [Property; 4] = [
    Property::BorderTopWidth,
    Property::BorderRightWidth,
    Property::BorderBottomWidth,
    Property::BorderLeftWidth,
];
fn half(edges: Edges) -> Edges {
    Edges {
        top: edges.top / 2.0,
        right: edges.right / 2.0,
        bottom: edges.bottom / 2.0,
        left: edges.left / 2.0,
    }
}
/// A content-box length from a CSS one, by box-sizing.
fn content_box(style: &ComputedStyle, value: f64, decoration: f64) -> f64 {
    if keyword(style, Property::BoxSizing) == "border-box" {
        (value - decoration).max(0.0)
    } else {
        value.max(0.0)
    }
}
fn percent(style: &ComputedStyle, p: Property) -> f64 {
    match style.get(p) {
        Some(ComputedValue::Length(Length::Percent(n))) => n.clamp(0.0, 100.0),
        _ => 0.0,
    }
}
/// HTML's rules for parsing non-negative integers.
fn attribute(styled: &StyledDocument<'_>, node: NodeId, name: &str) -> Option<i32> {
    let element = styled.document().node(node)?.element()?;
    crate::list::parse_integer(element.attribute(name)?).filter(|n| *n >= 0)
}
/// `span` on a column or column group: 1 to 1000, else 1.
fn span(styled: &StyledDocument<'_>, node: NodeId) -> u32 {
    let element = styled.document().node(node).and_then(|n| n.element());
    if !element.is_some_and(|e| matches!(e.name.as_str(), "col" | "colgroup")) {
        return 1;
    }
    attribute(styled, node, "span")
        .filter(|n| *n > 0)
        .map_or(1, |n| (n as u32).min(MAX_COLSPAN))
}

impl<M: FontMetrics> Engine<'_, '_, '_, '_, M> {
    /// A block's part in a table, by its display; None on a page with no
    /// table display at all.
    pub(super) fn role(&self, b: &BoxNode) -> Result<Option<TableRole>, Error> {
        if !self.styled.has_tables() || b.kind != BoxKind::Block {
            return Ok(None);
        }
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        Ok(TableRole::of(keyword(style, Property::Display)))
    }
    /// Text of HTML white space alone; charged the bytes looked at.
    fn blank(&mut self, b: &BoxNode) -> Result<bool, Error> {
        if b.kind != BoxKind::Text {
            return Ok(false);
        }
        let text = self
            .styled
            .document()
            .node(b.node)
            .and_then(|n| n.text())
            .ok_or(Error::Invalid)?;
        let solid = text
            .bytes()
            .position(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c'));
        self.counters.step(solid.map_or(text.len(), |n| n + 1))?;
        Ok(solid.is_none())
    }
    fn kind(&mut self, id: BoxId) -> Result<Kind, Error> {
        let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
        if self.blank(&b)? {
            return Ok(Kind::Blank);
        }
        Ok(match self.role(&b)? {
            Some(role) => Kind::Proper(role),
            None => Kind::Other,
        })
    }
    /// Where a run of table boxes with no table, from `at`, ends: blocks
    /// of an internal table display and the blank text between them.
    pub(super) fn table_run(
        &mut self,
        events: &Events,
        at: usize,
        role: Option<TableRole>,
    ) -> Result<Option<usize>, Error> {
        if role.is_none() || role == Some(TableRole::Table) {
            return Ok(None);
        }
        let mut end = at + 1;
        let mut next = end;
        while let Some(event) = events.get(next).copied() {
            let b = match event {
                Event::Block(id) | Event::Inline(id) | Event::Float(id) | Event::Atomic(id) => {
                    *self.styled.box_node(id).ok_or(Error::Invalid)?
                }
            };
            if matches!(self.role(&b)?, Some(r) if r != TableRole::Table) {
                next += 1;
                end = next;
            } else if self.blank(&b)? {
                next += 1;
            } else {
                break;
            }
        }
        Ok(Some(end))
    }

    /// Lays a table and its captions at `y`, returning their height. While
    /// any table is laid every line the window may yet meet is kept; the
    /// outermost keeps only the window's as its rows end and once it is
    /// done. No line of it is an inline-block's last (DESIGN.md §5,
    /// "Inline-blocks").
    pub(super) fn table(
        &mut self,
        source: Source<'_>,
        place: &Place,
        y: f64,
        depth: usize,
    ) -> Result<f64, Error> {
        self.laid_table(source, place, y, depth)
            .map(|(height, _)| height)
    }
    /// Lays the inline table `id`, `dims` in its container, at `y`, as
    /// `table` lays a table but for its auto margins, which are 0 (CSS 2.2
    /// §10.3.9), returning its height and its first row's baseline in
    /// flow (§17.4, §17.5.3), if it has one.
    pub(super) fn inline_table(
        &mut self,
        id: BoxId,
        dims: Dimensions,
        place: &Place,
        y: f64,
        depth: usize,
    ) -> Result<(f64, Option<f64>), Error> {
        self.laid_table(Source::Inline(id, dims), place, y, depth)
    }
    fn laid_table(
        &mut self,
        source: Source<'_>,
        place: &Place,
        y: f64,
        depth: usize,
    ) -> Result<(f64, Option<f64>), Error> {
        if depth > self.counters.limits.depth {
            return Err(Error::Limit("MAX_DEPTH"));
        }
        let outermost = self.retain == 0;
        if outermost {
            self.filtered = (self.lines.len(), self.markers.len());
        }
        self.retain = self.retain.checked_add(1).ok_or(Error::Invalid)?;
        let base = self.tables.base();
        let last_baseline = self.last_baseline;
        let laid = self.lay_table(source, place, y, depth);
        self.last_baseline = last_baseline;
        self.tables.truncate(base);
        self.retain -= 1;
        let laid = laid?;
        if outermost {
            self.keep_window()?;
        }
        Ok(laid)
    }
    /// The intrinsic widths of a table's border box, in 1/64 px.
    pub(super) fn table_intrinsic(
        &mut self,
        table: Measured<'_>,
        strut: StyleId,
        depth: usize,
    ) -> Result<(i64, i64), Error> {
        if depth > self.counters.limits.depth {
            return Err(Error::Limit("MAX_DEPTH"));
        }
        let base = self.tables.base();
        let measured = self.measure_table(table, strut, depth);
        self.tables.truncate(base);
        measured
    }
    fn measure_table(
        &mut self,
        table: Measured<'_>,
        strut: StyleId,
        depth: usize,
    ) -> Result<(i64, i64), Error> {
        self.counters.step(super::intrinsic::MEASURE_BOX)?;
        let (frame, walk) = match table {
            Measured::Root(id) => {
                let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
                let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
                let dims = sized(self.styled, &b, style, None, None, None)?;
                (
                    self.frame(Some((id, &dims)), strut, None, None)?,
                    Walk::chain(b.first_child, None),
                )
            }
            Measured::Anonymous(events, range) => (
                self.frame(None, strut, None, None)?,
                Walk::events(events, range),
            ),
        };
        let decoration = up(frame.decoration().horizontal());
        let bound = |w: i64| {
            w.clamp(
                units(frame.min_width),
                units(frame.max_width).max(units(frame.min_width)),
            )
        };
        let grid = self.grid(walk, frame.style)?;
        if frame.fixed {
            // As laid: the set width, or the columns' if they are wider.
            let width = bound(down(frame.width.unwrap_or(0.0)));
            let w = self.fixed_widths(grid, &frame, width)?.max(width) + decoration;
            return Ok((w, w));
        }
        let caption = self.measure(grid, &frame, depth)?;
        let (least, most) = self.extent(grid, &frame)?;
        let caption = (caption - decoration).max(0);
        let (least, most) = match frame.width {
            Some(width) => {
                let w = bound(units(width)).max(least).max(caption);
                (w, w)
            }
            None => (
                bound(least).max(least).max(caption),
                bound(most).max(least).max(caption),
            ),
        };
        Ok((least + decoration, most + decoration))
    }
    /// The table's own box: its style, spacing, edges and set sizes. A
    /// table with no box takes its parent's inherited values and initial
    /// others (CSS 2.2 §17.2.1).
    fn frame(
        &self,
        root: Option<(BoxId, &Dimensions)>,
        strut: StyleId,
        basis: Option<f64>,
        definite: Option<f64>,
    ) -> Result<Frame, Error> {
        let (node, style_id) = match root {
            Some((id, _)) => {
                let b = self.styled.box_node(id).ok_or(Error::Invalid)?;
                (Some((id, b.style)), b.style)
            }
            None => (None, strut),
        };
        let style = self.styled.style(style_id).ok_or(Error::Invalid)?;
        let collapse = keyword(style, Property::BorderCollapse) == "collapse";
        let (across, down) = match style.get(Property::BorderSpacing) {
            Some(ComputedValue::Spacing {
                horizontal,
                vertical,
            }) if !collapse => (*horizontal, *vertical),
            _ => (0.0, 0.0),
        };
        let (across, down) = (units(across.max(0.0)), px(units(down.max(0.0))));
        let Some((_, dims)) = root else {
            return Ok(Frame {
                node,
                style: style_id,
                collapse,
                across,
                down,
                padding: Edges::default(),
                border: Edges::default(),
                width: None,
                min_width: 0.0,
                max_width: f64::INFINITY,
                height: None,
                min_height: 0.0,
                max_height: f64::INFINITY,
                fixed: false,
            });
        };
        // Collapsed, a table has no padding and half its border.
        let (padding, border) = if collapse {
            (Edges::default(), half(dims.border))
        } else {
            (dims.padding, dims.border)
        };
        let decoration = padding.plus(border);
        let along = |p, basis: Option<f64>, decor: f64| {
            length(style, p, basis).map(|v| content_box(style, v, decor))
        };
        let width = along(Property::Width, basis, decoration.horizontal());
        Ok(Frame {
            node,
            style: style_id,
            collapse,
            across,
            down,
            padding,
            border,
            width,
            min_width: along(Property::MinWidth, basis, decoration.horizontal()).unwrap_or(0.0),
            max_width: along(Property::MaxWidth, basis, decoration.horizontal())
                .unwrap_or(f64::INFINITY),
            height: along(Property::Height, definite, decoration.vertical()),
            min_height: along(Property::MinHeight, definite, decoration.vertical()).unwrap_or(0.0),
            max_height: along(Property::MaxHeight, definite, decoration.vertical())
                .unwrap_or(f64::INFINITY),
            fixed: keyword(style, Property::TableLayout) == "fixed" && width.is_some(),
        })
    }

    /// Builds the grid of a table's children: captions, columns, rows in
    /// their groups and cells, with anonymous rows and cells around what
    /// no row or cell holds (CSS 2.2 §17.2.1), placed by HTML's table
    /// model (HTML §4.9.12), then merged into effective columns.
    fn grid(&mut self, mut walk: Walk<'_>, style: StyleId) -> Result<Grid, Error> {
        let grid = self.tables.base();
        let mut column = 0u32;
        // Rowspans end with their row group; each group marks its own.
        let mut group = 0u32;
        let mut implicit: Option<u32> = None;
        while let Some(id) = walk.peek(self.styled)? {
            self.counters.step(1)?;
            let kind = self.kind(id)?;
            match kind {
                Kind::Blank => walk.take(),
                Kind::Proper(TableRole::Caption) => {
                    walk.take();
                    self.close(grid, &mut implicit)?;
                    self.item(Item::Caption(id))?;
                }
                Kind::Proper(TableRole::Column | TableRole::ColumnGroup) => {
                    walk.take();
                    self.columns(id, grid, &mut column)?;
                }
                Kind::Proper(TableRole::RowGroup) => {
                    walk.take();
                    self.close(grid, &mut implicit)?;
                    group = group.checked_add(1).ok_or(Error::Invalid)?;
                    let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
                    let first = self.row_count(grid)?;
                    let mut rows = Walk::chain(b.first_child, None);
                    while let Some(child) = rows.peek(self.styled)? {
                        self.counters.step(1)?;
                        match self.kind(child)? {
                            Kind::Blank => rows.take(),
                            Kind::Proper(TableRole::Row) => {
                                rows.take();
                                let row = *self.styled.box_node(child).ok_or(Error::Invalid)?;
                                let mut cells = Walk::chain(row.first_child, None);
                                self.row(
                                    Some(child),
                                    row.style,
                                    &mut cells,
                                    Stop::Never,
                                    grid,
                                    group,
                                )?;
                            }
                            _ => self.row(None, b.style, &mut rows, Stop::Group, grid, group)?,
                        }
                    }
                    let end = self.row_count(grid)?;
                    self.end_group(grid, first, end)?;
                    self.item(Item::Group {
                        node: id,
                        first,
                        end,
                    })?;
                }
                _ => {
                    if implicit.is_none() {
                        group = group.checked_add(1).ok_or(Error::Invalid)?;
                        implicit = Some(self.row_count(grid)?);
                    }
                    if kind == Kind::Proper(TableRole::Row) {
                        walk.take();
                        let row = *self.styled.box_node(id).ok_or(Error::Invalid)?;
                        let mut cells = Walk::chain(row.first_child, None);
                        self.row(Some(id), row.style, &mut cells, Stop::Never, grid, group)?;
                    } else {
                        self.row(None, style, &mut walk, Stop::Table, grid, group)?;
                    }
                }
            }
        }
        self.close(grid, &mut implicit)?;
        self.effective(grid)?;
        Ok(grid)
    }
    fn row_count(&self, grid: Grid) -> Result<u32, Error> {
        u32::try_from(self.tables.rows.len() - grid.rows).map_err(|_| Error::Invalid)
    }
    fn item(&mut self, item: Item) -> Result<(), Error> {
        self.counters.admit_growth(&mut self.tables.items)?;
        self.tables.items.try_push(item)?;
        Ok(())
    }
    /// Ends the group rows directly in the table make, if open.
    fn close(&mut self, grid: Grid, implicit: &mut Option<u32>) -> Result<(), Error> {
        if let Some(first) = implicit.take() {
            let end = self.row_count(grid)?;
            self.end_group(grid, first, end)?;
        }
        Ok(())
    }
    /// Cuts the group's rowspans at its last row (HTML §4.9.12: a span of
    /// 0, or past the group, reaches its end).
    fn end_group(&mut self, grid: Grid, first: u32, end: u32) -> Result<(), Error> {
        let rows = self.tables.rows.get(grid.rows..).ok_or(Error::Invalid)?;
        let (Some(from), Some(to)) = (
            rows.get(first as usize).map(|r| r.cells),
            end.checked_sub(1)
                .and_then(|last| rows.get(last as usize))
                .map(|r| r.end),
        ) else {
            return Ok(());
        };
        self.counters.step(to.saturating_sub(from))?;
        for cell in self
            .tables
            .cells
            .get_mut(grid.cells + from..grid.cells + to)
            .ok_or(Error::Invalid)?
        {
            let left = end - cell.row;
            cell.rowspan = if cell.rowspan == 0 {
                left
            } else {
                cell.rowspan.min(left)
            };
        }
        Ok(())
    }
    /// A column box, or a group and its columns: columns of the grid from
    /// `column` on.
    fn columns(&mut self, id: BoxId, grid: Grid, column: &mut u32) -> Result<(), Error> {
        let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
        let first = *column;
        if self.role(&b)? == Some(TableRole::ColumnGroup) {
            let mut next = b.first_child;
            while let Some(child) = next {
                self.counters.step(1)?;
                let c = *self.styled.box_node(child).ok_or(Error::Invalid)?;
                // Anything else in a group is not rendered (§17.2.1).
                if self.role(&c)? == Some(TableRole::Column) {
                    self.column_item(child, c.node, grid, column)?;
                }
                next = c.next_sibling;
            }
        }
        if *column == first {
            // A column, or a group without any: its own span and width.
            return self.column_item(id, b.node, grid, column);
        }
        self.item(Item::Column {
            node: id,
            first,
            span: *column - first,
            own: false,
            at: usize::MAX,
        })
    }
    fn column_item(
        &mut self,
        id: BoxId,
        node: NodeId,
        grid: Grid,
        column: &mut u32,
    ) -> Result<(), Error> {
        let first = *column;
        let span = span(self.styled, node);
        *column = first.checked_add(span).ok_or(Error::Invalid)?;
        self.grow(grid, *column)?;
        self.item(Item::Column {
            node: id,
            first,
            span,
            own: true,
            at: usize::MAX,
        })
    }
    /// Columns up to `end`, refused past MAX_TABLE_COLUMNS.
    fn grow(&mut self, grid: Grid, end: u32) -> Result<(), Error> {
        if end as usize > MAX_TABLE_COLUMNS {
            return Err(Error::Limit("MAX_TABLE_COLUMNS"));
        }
        while self.tables.cols.len() - grid.cols < end as usize {
            self.counters.step(1)?;
            self.counters.admit_growth(&mut self.tables.cols)?;
            self.tables.cols.try_push(Col::default())?;
        }
        Ok(())
    }
    /// One row: its cells, and anonymous cells around runs of anything
    /// else, from `walk` until `stop`.
    fn row(
        &mut self,
        node: Option<BoxId>,
        strut: StyleId,
        walk: &mut Walk<'_>,
        stop: Stop,
        grid: Grid,
        group: u32,
    ) -> Result<(), Error> {
        let row = self.row_count(grid)?;
        let cells = self.tables.cells.len() - grid.cells;
        let mut column = 0u32;
        while let Some(id) = walk.peek(self.styled)? {
            self.counters.step(1)?;
            let kind = self.kind(id)?;
            if stop.at(kind) {
                break;
            }
            walk.take();
            match kind {
                Kind::Blank => {}
                Kind::Proper(TableRole::Cell) => {
                    let spans = self.spans(id)?;
                    self.place(Content::Cell(id), spans, row, &mut column, grid, group)?;
                }
                _ => {
                    let mut last = id;
                    if walk.chained() {
                        while let Some(next) = walk.peek(self.styled)? {
                            self.counters.step(1)?;
                            let kind = self.kind(next)?;
                            if stop.at(kind) || kind == Kind::Proper(TableRole::Cell) {
                                break;
                            }
                            walk.take();
                            last = next;
                        }
                    }
                    let until = self
                        .styled
                        .box_node(last)
                        .ok_or(Error::Invalid)?
                        .next_sibling;
                    self.place(
                        Content::Run(id, until),
                        (1, 1),
                        row,
                        &mut column,
                        grid,
                        group,
                    )?;
                }
            }
        }
        self.counters.admit_growth(&mut self.tables.rows)?;
        self.tables.rows.try_push(Row {
            node,
            strut,
            cells,
            end: self.tables.cells.len() - grid.cells,
            y: 0.0,
            height: 0.0,
        })?;
        self.item(Item::Row(row))
    }
    /// A td or th's `colspan` (1 to 1000) and `rowspan` (0 to 65534).
    fn spans(&self, id: BoxId) -> Result<(u32, u32), Error> {
        let b = self.styled.box_node(id).ok_or(Error::Invalid)?;
        let element = self
            .styled
            .document()
            .node(b.node)
            .and_then(|n| n.element());
        if !element.is_some_and(|e| matches!(e.name.as_str(), "td" | "th")) {
            return Ok((1, 1));
        }
        let colspan = attribute(self.styled, b.node, "colspan")
            .filter(|n| *n > 0)
            .map_or(1, |n| (n as u32).min(MAX_COLSPAN));
        let rowspan =
            attribute(self.styled, b.node, "rowspan").map_or(1, |n| (n as u32).min(MAX_ROWSPAN));
        Ok((colspan, rowspan))
    }
    /// Places a cell at the first column from `column` no rowspan above
    /// holds (HTML §4.9.12, "growing downward-growing cells").
    fn place(
        &mut self,
        content: Content,
        (colspan, rowspan): (u32, u32),
        row: u32,
        column: &mut u32,
        grid: Grid,
        group: u32,
    ) -> Result<(), Error> {
        let held = |col: &Col| col.group == group && col.until > row;
        while self
            .tables
            .cols
            .get(grid.cols + *column as usize)
            .is_some_and(held)
        {
            self.counters.step(1)?;
            *column += 1;
        }
        let end = column.checked_add(colspan).ok_or(Error::Invalid)?;
        self.grow(grid, end)?;
        // A span of 0 reaches the group's end, as one past it does.
        let until = if rowspan == 0 {
            u32::MAX
        } else {
            row.saturating_add(rowspan)
        };
        self.counters.step(colspan as usize)?;
        for col in self
            .tables
            .cols
            .get_mut(grid.cols + *column as usize..grid.cols + end as usize)
            .ok_or(Error::Invalid)?
        {
            // A cell overlapping a rowspan (HTML's table model error) does
            // not end it early.
            if col.group == group {
                col.until = col.until.max(until);
            } else {
                col.group = group;
                col.until = until;
            }
        }
        self.counters.admit_growth(&mut self.tables.cells)?;
        self.tables.cells.try_push(Cell {
            content,
            row,
            col: *column,
            colspan,
            rowspan,
            min: 0,
            max: 0,
            percent: 0.0,
            fixed: false,
        })?;
        *column = end;
        Ok(())
    }
    /// Merges grid columns no cell or column box starts or ends between
    /// into effective columns, as browsers do, so a `colspan=100` over
    /// three columns makes no ninety-seven empty ones with spacing.
    fn effective(&mut self, grid: Grid) -> Result<(), Error> {
        let count = self.tables.cols.len() - grid.cols;
        if count == 0 {
            return Ok(());
        }
        let cells = self.tables.cells.len() - grid.cells;
        let items = self.tables.items.len() - grid.items;
        self.counters.step(count + cells + items)?;
        let cols = self
            .tables
            .cols
            .get_mut(grid.cols..)
            .ok_or(Error::Invalid)?;
        let mut mark = |at: u32| {
            if let Some(col) = cols.get_mut(at as usize) {
                col.edge = true;
            }
        };
        mark(0);
        for cell in self.tables.cells.get(grid.cells..).ok_or(Error::Invalid)? {
            mark(cell.col);
            mark(cell.col.saturating_add(cell.colspan));
        }
        for item in self.tables.items.get(grid.items..).ok_or(Error::Invalid)? {
            if let Item::Column { first, span, .. } = item {
                mark(*first);
                mark(first.saturating_add(*span));
            }
        }
        let mut effective = 0u32;
        for (at, col) in cols.iter_mut().enumerate() {
            if col.edge && at > 0 {
                effective += 1;
            }
            col.effective = effective;
        }
        let map = |cols: &[Col], first: u32, span: u32| -> Result<(u32, u32), Error> {
            let start = cols.get(first as usize).ok_or(Error::Invalid)?.effective;
            let last = first
                .checked_add(span)
                .and_then(|end| end.checked_sub(1))
                .and_then(|last| cols.get(last as usize))
                .map_or(start, |c| c.effective);
            Ok((start, last + 1 - start))
        };
        for cell in self
            .tables
            .cells
            .get_mut(grid.cells..)
            .ok_or(Error::Invalid)?
        {
            (cell.col, cell.colspan) = map(cols, cell.col, cell.colspan)?;
        }
        for item in self
            .tables
            .items
            .get_mut(grid.items..)
            .ok_or(Error::Invalid)?
        {
            if let Item::Column { first, span, .. } = item {
                (*first, *span) = map(cols, *first, *span)?;
            }
        }
        for col in cols.iter_mut() {
            *col = Col::default();
        }
        self.tables
            .cols
            .truncate(grid.cols + effective as usize + 1);
        Ok(())
    }

    /// Each cell's intrinsic widths and the columns' (CSS 2.2 §17.5.2.2
    /// steps 1 and 2), and the widest caption's minimum, in 1/64 px.
    fn measure(&mut self, grid: Grid, frame: &Frame, depth: usize) -> Result<i64, Error> {
        let count = self.tables.cells.len() - grid.cells;
        self.counters
            .step(count.saturating_mul(super::intrinsic::MEASURE_BOX))?;
        for at in 0..count {
            let cell = *self
                .tables
                .cells
                .get(grid.cells + at)
                .ok_or(Error::Invalid)?;
            let Inside {
                first,
                until,
                strut,
                node,
            } = self.content(grid, &cell)?;
            let (least, most) = self.content_intrinsic(first, until, strut, depth + 3)?;
            let (mut min, mut max, mut share, mut fixed) = (least, most, 0.0, false);
            if let Some(id) = node {
                let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
                let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
                let (padding, border) = cell_edges(style, frame.collapse);
                let decoration = padding.plus(border).horizontal();
                min += up(decoration);
                max += up(decoration);
                let outer = |w: f64| up(content_box(style, w, decoration) + decoration);
                // min-width raises the floor; a width is the cell's preferred
                // width, never under its content's minimum (CSS Tables 3
                // §3.9.1's outer max-content), as browsers take it.
                if let Some(low) = length(style, Property::MinWidth, None) {
                    min = min.max(outer(low));
                }
                if let Some(width) = length(style, Property::Width, None) {
                    fixed = true;
                    max = outer(width);
                }
                share = percent(style, Property::Width);
            }
            let cell = self
                .tables
                .cells
                .get_mut(grid.cells + at)
                .ok_or(Error::Invalid)?;
            cell.min = min;
            cell.max = max.max(min);
            cell.percent = share;
            cell.fixed = fixed;
        }
        // Spanning cells narrowest first, so a wide one spreads over what
        // narrower ones gave, as browsers order them.
        let order = self.tables.order.len();
        for at in 0..count {
            let cell = self
                .tables
                .cells
                .get(grid.cells + at)
                .ok_or(Error::Invalid)?;
            if cell.colspan > 1 {
                let key = (cell.colspan, at);
                self.counters.admit_growth(&mut self.tables.order)?;
                self.tables.order.try_push(key)?;
            }
        }
        let spanning = self.tables.order.get_mut(order..).ok_or(Error::Invalid)?;
        let n = spanning.len();
        self.counters
            .step(n.saturating_mul(n.max(1).ilog2() as usize + 1))?;
        spanning.sort_unstable();
        let across = frame.across;
        let cols = self
            .tables
            .cols
            .get_mut(grid.cols..)
            .ok_or(Error::Invalid)?;
        let cells = self.tables.cells.get(grid.cells..).ok_or(Error::Invalid)?;
        // A column with a width set by a cell prefers the widest such, not
        // its other cells' content; `width` holds it until the columns
        // are given theirs.
        for cell in cells.iter().filter(|c| c.colspan == 1) {
            let col = cols.get_mut(cell.col as usize).ok_or(Error::Invalid)?;
            col.min = col.min.max(cell.min);
            if cell.fixed {
                col.width = col.width.max(cell.max);
            } else {
                col.max = col.max.max(cell.max);
            }
            col.percent = col.percent.max(cell.percent);
            col.fixed |= cell.fixed;
        }
        for col in cols.iter_mut() {
            if col.fixed {
                col.max = col.width;
            }
            col.width = 0;
            col.max = col.max.max(col.min);
        }
        // A column box's width is its columns' preferred width, as a
        // cell's is.
        let items = self.tables.items.get(grid.items..).ok_or(Error::Invalid)?;
        for item in items {
            let Item::Column {
                node,
                first,
                span,
                own: true,
                ..
            } = *item
            else {
                continue;
            };
            let b = self.styled.box_node(node).ok_or(Error::Invalid)?;
            let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
            let width = length(style, Property::Width, None).map(|w| units(w.max(0.0)));
            let share = percent(style, Property::Width) / f64::from(span.max(1));
            for col in cols
                .get_mut(first as usize..(first + span) as usize)
                .ok_or(Error::Invalid)?
            {
                if let Some(width) = width {
                    col.max = width.max(col.min);
                    col.fixed = true;
                }
                col.percent = col.percent.max(share);
            }
        }
        let mut work = 0usize;
        for &(_, at) in self.tables.order.get(order..).ok_or(Error::Invalid)? {
            let cell = cells.get(at).ok_or(Error::Invalid)?;
            let span = cols
                .get_mut(cell.col as usize..(cell.col + cell.colspan) as usize)
                .ok_or(Error::Invalid)?;
            work = work.saturating_add(6 * span.len());
            let gaps = across * i64::from(cell.colspan - 1);
            spread(span, cell.min - gaps, |c| &mut c.min);
            spread(span, cell.max - gaps, |c| &mut c.max);
            // Its percentage over columns with none, by their maxima.
            if cell.percent > 0.0 && span.iter().all(|c| c.percent == 0.0) {
                let weights: i64 = span.iter().map(|c| c.max).sum();
                let count = span.len() as f64;
                for col in span.iter_mut() {
                    col.percent = if weights > 0 {
                        cell.percent * col.max as f64 / weights as f64
                    } else {
                        cell.percent / count
                    };
                }
            }
            for col in span.iter_mut() {
                col.fixed |= cell.fixed && col.percent == 0.0;
                col.max = col.max.max(col.min);
            }
        }
        self.tables.order.truncate(order);
        self.counters.step(work)?;
        let mut caption = 0i64;
        let items = self.tables.items.len() - grid.items;
        for at in 0..items {
            if let Some(Item::Caption(id)) = self.tables.items.get(grid.items + at).copied() {
                let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
                let (least, _) = self.block_intrinsic(id, b, None, depth + 1)?;
                caption = caption.max(least);
            }
        }
        Ok(caption)
    }
    /// A cell's content: its first box, the box it stops at, the style an
    /// anonymous one is laid with, and its own box.
    fn content(&self, grid: Grid, cell: &Cell) -> Result<Inside, Error> {
        Ok(match cell.content {
            Content::Cell(id) => {
                let b = self.styled.box_node(id).ok_or(Error::Invalid)?;
                Inside {
                    first: b.first_child,
                    until: None,
                    strut: b.style,
                    node: Some(id),
                }
            }
            Content::Run(first, until) => {
                let row = self
                    .tables
                    .rows
                    .get(grid.rows + cell.row as usize)
                    .ok_or(Error::Invalid)?;
                Inside {
                    first: Some(first),
                    until,
                    strut: row.strut,
                    node: None,
                }
            }
        })
    }
    /// The columns' least and preferred content widths, spacing included:
    /// a percentage column asks the width its maximum is that share of,
    /// and the other columns the rest (as browsers resolve percentages).
    fn extent(&mut self, grid: Grid, frame: &Frame) -> Result<(i64, i64), Error> {
        let cols = self.tables.cols.get(grid.cols..).ok_or(Error::Invalid)?;
        self.counters.step(cols.len())?;
        if cols.is_empty() {
            return Ok((0, 0));
        }
        let gaps = frame.across * (cols.len() as i64 + 1);
        let least: i64 = cols.iter().map(|c| c.min).sum();
        let mut most: f64 = cols.iter().map(|c| c.max as f64).sum();
        let mut share = 0.0;
        let mut rest = 0.0;
        for col in cols {
            if col.percent > 0.0 && share < 100.0 {
                let p = col.percent.min(100.0 - share);
                share += p;
                most = most.max(col.max as f64 * 100.0 / p);
            } else {
                rest += col.max as f64;
            }
        }
        if share > 0.0 {
            most = most.max(if share < 100.0 {
                rest * 100.0 / (100.0 - share)
            } else if rest > 0.0 {
                MAX_VIEWPORT * 64.0
            } else {
                0.0
            });
        }
        let most = (most.min(MAX_VIEWPORT * 64.0) as i64).max(least);
        Ok((least + gaps, most + gaps))
    }
    /// CSS 2.2 §17.5.2.1: columns from column boxes, then the first row's
    /// cells, by width; the rest share what is left. No cell is measured.
    fn fixed_widths(&mut self, grid: Grid, frame: &Frame, width: i64) -> Result<i64, Error> {
        let count = self.tables.cols.len() - grid.cols;
        let gaps = frame.across * (count as i64 + 1);
        let room = (width - gaps).max(0);
        let mut set = 0usize;
        let items = self.tables.items.len() - grid.items;
        for at in 0..items {
            let Some(Item::Column {
                node,
                first,
                span,
                own: true,
                ..
            }) = self.tables.items.get(grid.items + at).copied()
            else {
                continue;
            };
            let b = self.styled.box_node(node).ok_or(Error::Invalid)?;
            let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
            if let Some(w) = length(style, Property::Width, Some(px(room))) {
                let each = units(w.max(0.0)) / i64::from(span.max(1));
                self.set_columns(grid, first, span, each, &mut set)?;
            }
        }
        let cells = self.tables.cells.len() - grid.cells;
        for at in 0..cells {
            let cell = *self
                .tables
                .cells
                .get(grid.cells + at)
                .ok_or(Error::Invalid)?;
            if cell.row != 0 {
                break;
            }
            let Content::Cell(id) = cell.content else {
                continue;
            };
            let b = self.styled.box_node(id).ok_or(Error::Invalid)?;
            let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
            if let Some(w) = length(style, Property::Width, Some(px(room))) {
                let (padding, border) = cell_edges(style, frame.collapse);
                let decoration = padding.plus(border).horizontal();
                let w = content_box(style, w, decoration) + decoration;
                let gaps = frame.across * i64::from(cell.colspan - 1);
                let each = (units(w) - gaps).max(0) / i64::from(cell.colspan);
                self.set_columns(grid, cell.col, cell.colspan, each, &mut set)?;
            }
        }
        let cols = self
            .tables
            .cols
            .get_mut(grid.cols..)
            .ok_or(Error::Invalid)?;
        self.counters.step(cols.len())?;
        let taken: i64 = cols.iter().filter(|c| c.fixed).map(|c| c.width).sum();
        let free = count - set;
        let left = (room - taken).max(0);
        if free == 0 && left > 0 {
            // Every column set and the table wider: the rest by width.
            give(cols, left, |_| true, |c| c.width);
        }
        let mut given = 0;
        let mut seen = 0;
        for col in cols.iter_mut().filter(|c| !c.fixed) {
            seen += 1;
            col.width = if seen == free {
                left - given
            } else {
                left / free as i64
            };
            given += col.width;
        }
        Ok(cols.iter().map(|c| c.width).sum::<i64>() + gaps)
    }
    fn set_columns(
        &mut self,
        grid: Grid,
        first: u32,
        span: u32,
        each: i64,
        set: &mut usize,
    ) -> Result<(), Error> {
        self.counters.step(span as usize)?;
        for col in self
            .tables
            .cols
            .get_mut(grid.cols + first as usize..grid.cols + (first + span) as usize)
            .ok_or(Error::Invalid)?
        {
            if !col.fixed {
                col.fixed = true;
                col.width = each;
                *set += 1;
            }
        }
        Ok(())
    }

    fn lay_table(
        &mut self,
        source: Source<'_>,
        place: &Place,
        y: f64,
        depth: usize,
    ) -> Result<(f64, Option<f64>), Error> {
        let container = place.container;
        let inline = matches!(source, Source::Inline(..));
        let (frame, walk, margin, relative) = match source {
            Source::Root(id, dims) | Source::Inline(id, dims) => {
                let b = self.styled.box_node(id).ok_or(Error::Invalid)?;
                let frame = self.frame(
                    Some((id, &dims)),
                    place.strut,
                    Some(container.width),
                    place.definite,
                )?;
                (
                    frame,
                    Walk::chain(b.first_child, None),
                    dims.margin,
                    dims.relative,
                )
            }
            Source::Anonymous(events, range) => (
                self.frame(None, place.strut, None, None)?,
                Walk::events(events, range),
                Edges::default(),
                (0.0, 0.0),
            ),
        };
        let offset = (place.offset.0 + relative.0, place.offset.1 + relative.1);
        let decoration = frame.decoration();
        let grid = self.grid(walk, frame.style)?;
        // The content width: set, else the columns' preferred within the
        // room the margins leave, never under their least (§17.5.2).
        // Its own margins, None where auto, not those block sizing over-
        // constrained for a stretched width.
        let (left, right) = match frame.node {
            Some((_, style)) => {
                let style = self.styled.style(style).ok_or(Error::Invalid)?;
                (
                    length(style, Property::MarginLeft, Some(container.width)),
                    length(style, Property::MarginRight, Some(container.width)),
                )
            }
            None => (Some(0.0), Some(0.0)),
        };
        // An inline table's auto margins are 0 (CSS 2.2 §10.3.9).
        let (left_auto, right_auto) = (left.is_none() && !inline, right.is_none() && !inline);
        let fixed_margins = left.unwrap_or(0.0) + right.unwrap_or(0.0);
        let room = (container.width - fixed_margins - decoration.horizontal()).max(0.0);
        let bound = |w: f64| w.min(frame.max_width).max(frame.min_width);
        let content = if frame.fixed {
            let width = down(bound(frame.width.unwrap_or(0.0)));
            self.fixed_widths(grid, &frame, width)?.max(width)
        } else {
            let caption = self.measure(grid, &frame, depth)?;
            let (least, most) = self.extent(grid, &frame)?;
            let least = least.max(caption - units(decoration.horizontal()));
            let wanted = match frame.width {
                Some(width) => down(bound(width)),
                None => down(bound(px(most).min(room))),
            };
            let width = wanted.max(least);
            self.auto_widths(grid, &frame, width)?;
            width
        };
        let ncols = self.tables.cols.len() - grid.cols;
        let width = px(content) + decoration.horizontal();
        if !coordinate(width) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        // Auto margins share what the table leaves (§10.3.3).
        let free = (container.width - width - fixed_margins).max(0.0);
        let margin_left = match (left_auto, right_auto) {
            (true, true) => free / 2.0,
            (true, false) => free,
            (false, _) => left.unwrap_or(0.0),
        };
        let margin_right = match (left_auto, right_auto) {
            (true, true) => free / 2.0,
            (false, true) => free,
            (_, false) => right.unwrap_or(0.0),
        };
        let x = container.x + margin_left;
        let content_x = x + decoration.left;
        // Columns' x from the content edge.
        let mut at = frame.across;
        self.counters.step(ncols)?;
        for col in self
            .tables
            .cols
            .get_mut(grid.cols..)
            .ok_or(Error::Invalid)?
        {
            col.x = at;
            at = at.saturating_add(col.width).saturating_add(frame.across);
        }
        let ctx = Context {
            grid,
            content_x,
            content_width: px(content),
            down: frame.down,
            collapse: frame.collapse,
            offset,
        };
        let items = self.tables.items.len() - grid.items;
        let rows = self.tables.rows.len() - grid.rows;
        let mut cursor = y;
        let mut item = 0;
        // Captions before the first row lay above the table box.
        while let Some(next) = self.tables.items.get(grid.items + item).copied() {
            match next {
                Item::Caption(id) => cursor += self.caption(id, x, cursor, width, offset, depth)?,
                Item::Column { .. } => self.column_box(grid.items + item, offset)?,
                Item::Row(_) | Item::Group { .. } => break,
            }
            item += 1;
        }
        let top = cursor;
        let content_top = top + decoration.top;
        let mut row_y = content_top + if rows > 0 { frame.down } else { 0.0 };
        let rows_top = row_y;
        let mut below = false;
        // An inline table's first row gives its baseline.
        let (mut first_row, mut row_baseline) = (inline, None);
        while item < items {
            match self
                .tables
                .items
                .get(grid.items + item)
                .copied()
                .ok_or(Error::Invalid)?
            {
                Item::Row(row) if first_row => {
                    first_row = false;
                    row_y = self.lay_row(&ctx, row, row_y, depth, Some(&mut row_baseline))?;
                }
                Item::Row(row) => row_y = self.lay_row(&ctx, row, row_y, depth, None)?,
                Item::Group { node, first, end } => {
                    self.group_box(&ctx, node, first, end, row_y)?
                }
                Item::Column { .. } => self.column_box(grid.items + item, offset)?,
                Item::Caption(_) => below = true,
            }
            item += 1;
        }
        let rows_bottom = if rows > 0 { row_y - frame.down } else { row_y };
        let laid = (row_y - content_top).max(0.0);
        let inner = frame
            .height
            .unwrap_or(0.0)
            .max(laid)
            .min(frame.max_height)
            .max(frame.min_height)
            .max(laid);
        let bottom = content_top + inner + decoration.bottom;
        if !coordinate(bottom) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        let mut cursor = bottom;
        if below {
            let mut seen_row = false;
            for at in 0..items {
                match self.tables.items.get(grid.items + at).copied() {
                    Some(Item::Row(_) | Item::Group { .. }) => seen_row = true,
                    Some(Item::Caption(id)) if seen_row => {
                        cursor += self.caption(id, x, cursor, width, offset, depth)?;
                    }
                    _ => {}
                }
            }
        }
        // Columns span the rows; patched now their extent is known.
        for at in 0..items {
            if let Some(Item::Column {
                first, span, at: g, ..
            }) = self.tables.items.get(grid.items + at).copied()
            {
                let (left, width) = self.span_x(&ctx, first, span)?;
                let rect = Rect {
                    x: left + offset.0,
                    y: rows_top + offset.1,
                    width,
                    height: (rows_bottom - rows_top).max(0.0),
                };
                let geometry = self.boxes.get_mut(g).ok_or(Error::Invalid)?;
                geometry.content = rect;
                geometry.border = rect;
                self.bounds(rect)?;
            }
        }
        if let Some((node, style_id)) = frame.node {
            let b = *self.styled.box_node(node).ok_or(Error::Invalid)?;
            let geometry = BoxGeometry {
                node: b.node,
                style: style_id,
                content: Rect {
                    x: content_x + offset.0,
                    y: content_top + offset.1,
                    width: px(content),
                    height: inner,
                },
                border: Rect {
                    x: x + offset.0,
                    y: top + offset.1,
                    width,
                    height: bottom - top,
                },
                padding: frame.padding,
                border_widths: frame.border,
                margin: Edges {
                    left: margin_left,
                    right: margin_right,
                    ..margin
                },
                replaced: false,
            };
            self.bounds(geometry.border)?;
            self.counters.admit_growth(&mut self.boxes)?;
            self.boxes.try_push(geometry)?;
        }
        let height = cursor - y;
        if !coordinate(height) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        Ok((height, row_baseline))
    }
    /// Distributes the content width, less spacing, over the columns:
    /// each starts at its maximum, or its share of the table for a
    /// percentage; extra goes to auto columns by their maxima, a shortfall
    /// is taken from auto columns, then those with a width, then
    /// percentages, each down to its minimum (§17.5.2.2 step 3).
    fn auto_widths(&mut self, grid: Grid, frame: &Frame, width: i64) -> Result<(), Error> {
        let cols = self
            .tables
            .cols
            .get_mut(grid.cols..)
            .ok_or(Error::Invalid)?;
        if cols.is_empty() {
            return Ok(());
        }
        let room = (width - frame.across * (cols.len() as i64 + 1)).max(0);
        let mut share = 0.0;
        for col in cols.iter_mut() {
            let p = col.percent.min(100.0 - share).max(0.0);
            share += p;
            col.percent = p;
            col.width = if p > 0.0 {
                col.min.max(units(p / 100.0 * px(room)))
            } else {
                col.max.max(col.min)
            };
        }
        let sum: i64 = cols.iter().map(|c| c.width).sum();
        let classes: [fn(&Col) -> bool; 3] = [
            |c| c.percent == 0.0 && !c.fixed,
            |c| c.percent == 0.0 && c.fixed,
            |c| c.percent > 0.0,
        ];
        if sum < room {
            let extra = room - sum;
            for (at, class) in classes.into_iter().enumerate() {
                if cols.iter().any(class) {
                    // Percentage columns alone share it by percentage.
                    let weight: fn(&Col) -> i64 = if at == 2 {
                        |c| (c.percent * 1024.0) as i64
                    } else {
                        |c| c.max
                    };
                    give(cols, extra, class, weight);
                    break;
                }
            }
        } else {
            let mut short = sum - room;
            for class in classes {
                if short <= 0 {
                    break;
                }
                let slack: i64 = cols
                    .iter()
                    .filter(|c| class(c))
                    .map(|c| c.width - c.min)
                    .sum();
                let take = short.min(slack);
                if take > 0 {
                    take_from(cols, take, slack, class);
                    short -= take;
                }
            }
        }
        self.counters.step(8 * cols.len())?;
        Ok(())
    }
    /// The x and width of `span` effective columns from `first`, the
    /// spacing between them included. The width is the columns' own, in
    /// whole 1/64 px, not a difference of two x's, which at a fractional
    /// table x falls an ulp short and wraps a cell measured to fit.
    fn span_x(&self, ctx: &Context, first: u32, span: u32) -> Result<(f64, f64), Error> {
        let cols = self
            .tables
            .cols
            .get(ctx.grid.cols..)
            .ok_or(Error::Invalid)?;
        let Some(start) = cols.get(first as usize) else {
            return Ok((ctx.content_x + ctx.content_width, 0.0));
        };
        let last = first
            .checked_add(span.max(1))
            .and_then(|e| e.checked_sub(1))
            .and_then(|l| cols.get(l as usize))
            .unwrap_or(start);
        let width = last.x.saturating_add(last.width).saturating_sub(start.x);
        Ok((ctx.content_x + px(start.x), px(width.max(0))))
    }
    /// A column box's geometry, in tree order; its rectangle once the
    /// rows are laid.
    fn column_box(&mut self, item: usize, offset: (f64, f64)) -> Result<(), Error> {
        let Some(Item::Column { node, .. }) = self.tables.items.get(item).copied() else {
            return Err(Error::Invalid);
        };
        let b = self.styled.box_node(node).ok_or(Error::Invalid)?;
        let at = self.boxes.len();
        self.counters.admit_growth(&mut self.boxes)?;
        self.boxes.try_push(BoxGeometry {
            node: b.node,
            style: b.style,
            content: Rect {
                x: offset.0,
                y: offset.1,
                ..Rect::default()
            },
            border: Rect::default(),
            padding: Edges::default(),
            border_widths: Edges::default(),
            margin: Edges::default(),
            replaced: false,
        })?;
        if let Some(Item::Column { at: slot, .. }) = self.tables.items.get_mut(item) {
            *slot = at;
        }
        Ok(())
    }
    fn group_box(
        &mut self,
        ctx: &Context,
        node: BoxId,
        first: u32,
        end: u32,
        next_y: f64,
    ) -> Result<(), Error> {
        let rows = self
            .tables
            .rows
            .get(ctx.grid.rows..)
            .ok_or(Error::Invalid)?;
        let (top, bottom) = match (
            rows.get(first as usize),
            end.checked_sub(1).and_then(|l| rows.get(l as usize)),
        ) {
            (Some(f), Some(l)) if first < end => (f.y, l.y + l.height),
            _ => (next_y, next_y),
        };
        let b = *self.styled.box_node(node).ok_or(Error::Invalid)?;
        self.band(ctx, b, top, bottom)
    }
    /// A row or group box: across the columns, from `top` to `bottom`.
    fn band(&mut self, ctx: &Context, b: BoxNode, top: f64, bottom: f64) -> Result<(), Error> {
        let count = self.tables.cols.len() - ctx.grid.cols;
        let (left, width) = if count > 0 {
            self.span_x(ctx, 0, count as u32)?
        } else {
            (ctx.content_x, 0.0)
        };
        let rect = Rect {
            x: left + ctx.offset.0,
            y: top + ctx.offset.1,
            width,
            height: (bottom - top).max(0.0),
        };
        self.bounds(rect)?;
        self.counters.admit_growth(&mut self.boxes)?;
        self.boxes.try_push(BoxGeometry {
            node: b.node,
            style: b.style,
            content: rect,
            border: rect,
            padding: Edges::default(),
            border_widths: Edges::default(),
            margin: Edges::default(),
            replaced: false,
        })?;
        Ok(())
    }
    /// A caption as a block the table's width, returning the height it
    /// takes with its margins.
    fn caption(
        &mut self,
        id: BoxId,
        x: f64,
        y: f64,
        width: f64,
        offset: (f64, f64),
        depth: usize,
    ) -> Result<f64, Error> {
        let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
        let dims = self.block_dimensions(&b, width, None)?;
        let height = self.block(id, b, dims, (x, y + dims.margin.top), offset, depth + 1)?;
        let taken = dims.margin.top + height + dims.margin.bottom;
        if !coordinate(taken) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        Ok(taken.max(0.0))
    }
    /// Lays a row's cells at `y`, sets its height from them and finishes
    /// the cells that end in it, returning the next row's y; given
    /// `watch`, its baseline there too (`row_baseline`).
    fn lay_row(
        &mut self,
        ctx: &Context,
        row: u32,
        y: f64,
        depth: usize,
        watch: Option<&mut Option<f64>>,
    ) -> Result<f64, Error> {
        let at = ctx.grid.rows + row as usize;
        let record = *self.tables.rows.get(at).ok_or(Error::Invalid)?;
        if let Some(r) = self.tables.rows.get_mut(at) {
            r.y = y;
        }
        for index in record.cells..record.end {
            let cell = *self
                .tables
                .cells
                .get(ctx.grid.cells + index)
                .ok_or(Error::Invalid)?;
            let (left, width) = self.span_x(ctx, cell.col, cell.colspan)?;
            let laid = self.lay_cell(ctx, &cell, left, width, y, depth)?;
            self.counters.admit_growth(&mut self.tables.laid)?;
            self.tables.laid.try_push(laid)?;
        }
        let set = match record.node {
            Some(id) => {
                let b = self.styled.box_node(id).ok_or(Error::Invalid)?;
                let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
                length(style, Property::Height, None)
                    .unwrap_or(0.0)
                    .max(0.0)
            }
            None => 0.0,
        };
        let rows = self
            .tables
            .rows
            .get(ctx.grid.rows..)
            .ok_or(Error::Invalid)?;
        let open = self
            .tables
            .laid
            .get(ctx.grid.laid..)
            .ok_or(Error::Invalid)?;
        self.counters.step(2 * open.len())?;
        // The row's baseline: the lowest of its baseline cells' (§17.5.3).
        let baseline = open
            .iter()
            .filter(|l| l.top == row && l.last == row && l.align == Align::Baseline)
            .map(|l| l.baseline)
            .fold(0.0, f64::max);
        let mut height = set;
        for laid in open.iter().filter(|l| l.last == row) {
            let needed = if laid.top == row {
                if laid.align == Align::Baseline {
                    // Its content moved to the baseline, or its set height.
                    (baseline - laid.baseline + laid.content).max(laid.height)
                } else {
                    laid.height
                }
            } else {
                // A rowspan ending here takes what its rows above leave.
                let above = rows.get(laid.top as usize).map_or(y, |r| r.y);
                laid.height - (y - above)
            };
            height = height.max(needed);
        }
        let height = px(units(height));
        if let Some(r) = self.tables.rows.get_mut(at) {
            r.height = height;
        }
        if let Some(out) = watch {
            *out = self.row_baseline(ctx, row, y, height, baseline)?;
        }
        let mut keep = ctx.grid.laid;
        let count = self.tables.laid.len();
        for read in ctx.grid.laid..count {
            let laid = *self.tables.laid.get(read).ok_or(Error::Invalid)?;
            if laid.last != row {
                self.tables.laid.swap(keep, read);
                keep += 1;
                continue;
            }
            let top = self
                .tables
                .rows
                .get(ctx.grid.rows + laid.top as usize)
                .map_or(y, |r| r.y);
            let cell = y + height - top;
            let free = (cell - laid.content).max(0.0);
            let shift = match laid.align {
                Align::Top => 0.0,
                Align::Middle => free / 2.0,
                Align::Bottom => free,
                Align::Baseline if laid.top == row => baseline - laid.baseline,
                Align::Baseline => 0.0,
            };
            self.finish_cell(&laid, cell, px(units(shift.max(0.0))))?;
        }
        self.tables.laid.truncate(keep);
        // The outermost table's rows are final once no rowspan is open.
        if self.retain == 1 && keep == ctx.grid.laid {
            self.keep_window()?;
        }
        if let Some(id) = record.node {
            let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
            self.band(ctx, b, y, y + height)?;
        }
        let next = y + height + ctx.down;
        if !coordinate(next) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        Ok(next)
    }
    /// The baseline of the row `row` laid at `y`, `height` tall, in flow,
    /// before its cells end (CSS 2.2 §17.5.3): its baseline cells', at
    /// `baseline` from its top as they were aligned, else the bottom
    /// content edge of its lowest cell; None where no cell starts and ends
    /// in it.
    #[cold]
    #[inline(never)]
    fn row_baseline(
        &mut self,
        ctx: &Context,
        row: u32,
        y: f64,
        height: f64,
        baseline: f64,
    ) -> Result<Option<f64>, Error> {
        let open = self
            .tables
            .laid
            .get(ctx.grid.laid..)
            .ok_or(Error::Invalid)?;
        self.counters.step(open.len())?;
        let own = |l: &&Laid| l.top == row && l.last == row;
        if open.iter().filter(own).any(|l| l.align == Align::Baseline) {
            return Ok(Some(y + baseline));
        }
        let mut lowest: Option<f64> = None;
        for laid in open.iter().filter(own) {
            // Its box is the row's height; its content edge within it.
            let below = match laid.geometry {
                Some(at) => {
                    let g = self.boxes.get(at).ok_or(Error::Invalid)?;
                    g.padding.bottom + g.border_widths.bottom
                }
                None => 0.0,
            };
            let bottom = y + height - below;
            lowest = Some(lowest.map_or(bottom, |l: f64| l.max(bottom)));
        }
        Ok(lowest)
    }
    /// Lays a cell's content at the top of its row, keeping what it laid
    /// to be moved once its rows are done.
    fn lay_cell(
        &mut self,
        ctx: &Context,
        cell: &Cell,
        x: f64,
        width: f64,
        y: f64,
        depth: usize,
    ) -> Result<Laid, Error> {
        let Inside {
            first,
            until,
            strut,
            node,
        } = self.content(ctx.grid, cell)?;
        let (padding, border, set, align) = match node {
            Some(id) => {
                let b = self.styled.box_node(id).ok_or(Error::Invalid)?;
                let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
                let (padding, border) = cell_edges(style, ctx.collapse);
                let set = length(style, Property::Height, None)
                    .map(|h| content_box(style, h, padding.plus(border).vertical()));
                let align = match keyword(style, Property::VerticalAlign) {
                    "top" => Align::Top,
                    "middle" => Align::Middle,
                    "bottom" => Align::Bottom,
                    // Every other value aligns a cell at the baseline.
                    _ => Align::Baseline,
                };
                (padding, border, set, align)
            }
            None => (Edges::default(), Edges::default(), None, Align::Baseline),
        };
        let decoration = padding.plus(border);
        let content = Rect {
            x: x + decoration.left,
            y: y + decoration.top,
            width: (width - decoration.horizontal()).max(0.0),
            height: 0.0,
        };
        let (lines, markers, boxes) = (self.lines.len(), self.markers.len(), self.boxes.len());
        let first_line = std::mem::replace(&mut self.first_line, true);
        let first_index = self.first_index.take();
        // The cell's own overflow, moved with it.
        let outer = (self.left, self.top, self.right, self.bottom);
        (self.left, self.top, self.right, self.bottom) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        // A cell roots a formatting context, which holds its floats.
        let context = self.styled.has_floats();
        if context {
            self.floats.open(&mut self.counters)?;
        }
        let laid = self.children(
            first,
            until,
            Place {
                container: content,
                definite: None,
                offset: ctx.offset,
                strut,
                // An anonymous cell's lines name the box holding its run.
                block: node.or_else(|| {
                    first
                        .and_then(|f| self.styled.box_node(f))
                        .and_then(|b| b.parent)
                }),
            },
            depth + 3,
            None,
        );
        let laid = match (laid, context) {
            (Ok(laid), true) => match self.floats.close()? {
                Some(bottom) => Ok(laid.max(bottom - content.y)),
                None => Ok(laid),
            },
            (laid, _) => laid,
        };
        let own = (self.left, self.top, self.right, self.bottom);
        (self.left, self.top, self.right, self.bottom) = outer;
        // An enclosing cell whose first line this was has it.
        let taken = std::mem::replace(&mut self.first_index, first_index);
        if first_line && taken.is_some() {
            self.first_index = taken;
        }
        self.first_line = first_line && taken.is_none();
        let laid = laid?;
        let bounds = (own.0 <= own.2).then_some(Rect {
            x: own.0,
            y: own.1,
            width: own.2 - own.0,
            height: own.3 - own.1,
        });
        let inner = laid.max(set.unwrap_or(0.0));
        let height = inner + decoration.vertical();
        let baseline = decoration.top
            + match taken.and_then(|at| self.lines.get(at)) {
                Some(line) => line.y + line.baseline - (content.y + ctx.offset.1),
                None => laid,
            };
        let geometry = match node {
            Some(id) => {
                let b = self.styled.box_node(id).ok_or(Error::Invalid)?;
                let at = self.boxes.len();
                self.counters.admit_growth(&mut self.boxes)?;
                self.boxes.try_push(BoxGeometry {
                    node: b.node,
                    style: b.style,
                    content: Rect {
                        x: content.x + ctx.offset.0,
                        y: content.y + ctx.offset.1,
                        width: content.width,
                        height: inner,
                    },
                    border: Rect {
                        x: x + ctx.offset.0,
                        y: y + ctx.offset.1,
                        width,
                        height,
                    },
                    padding,
                    border_widths: border,
                    margin: Edges::default(),
                    replaced: false,
                })?;
                Some(at)
            }
            None => None,
        };
        let last = cell
            .row
            .checked_add(cell.rowspan.max(1) - 1)
            .ok_or(Error::Invalid)?;
        Ok(Laid {
            top: cell.row,
            last,
            lines: (lines, self.lines.len()),
            boxes: (boxes, geometry.unwrap_or(self.boxes.len())),
            markers: (markers, self.markers.len()),
            geometry,
            height,
            content: laid + decoration.vertical(),
            baseline,
            align,
            bounds,
        })
    }
    /// Moves a laid cell's content down by `shift` and gives the cell its
    /// rows' height.
    fn finish_cell(&mut self, laid: &Laid, height: f64, shift: f64) -> Result<(), Error> {
        if shift != 0.0 {
            let (lines, boxes, markers) = (laid.lines, laid.boxes, laid.markers);
            self.counters
                .step((lines.1 - lines.0) + (boxes.1 - boxes.0) + (markers.1 - markers.0))?;
            for line in self.lines.get_mut(lines.0..lines.1).ok_or(Error::Invalid)? {
                line.y += shift;
                for image in &mut line.replaced {
                    image.y += shift;
                }
                if !coordinate(line.y + line.height) {
                    return Err(Error::Limit("MAX_COORDINATE"));
                }
            }
            for geometry in self.boxes.get_mut(boxes.0..boxes.1).ok_or(Error::Invalid)? {
                geometry.content.y += shift;
                geometry.border.y += shift;
            }
            for marker in self
                .markers
                .get_mut(markers.0..markers.1)
                .ok_or(Error::Invalid)?
            {
                marker.y += shift;
                marker.baseline += shift;
            }
        }
        if let Some(at) = laid.geometry {
            let geometry = self.boxes.get_mut(at).ok_or(Error::Invalid)?;
            let decoration = geometry.padding.plus(geometry.border_widths);
            geometry.border.height = height;
            geometry.content.height = (height - decoration.vertical()).max(0.0);
            let border = geometry.border;
            self.bounds(border)?;
        }
        if let Some(rect) = laid.bounds {
            self.bounds(Rect {
                y: rect.y + shift,
                ..rect
            })?;
        }
        Ok(())
    }
    /// Keeps, of what the outermost table laid since it last filtered, the
    /// lines and markers the window meets, as a layout that never held
    /// them would.
    pub(super) fn keep_window(&mut self) -> Result<(), Error> {
        let (lines, markers) = self.filtered;
        let Some(window) = self.request.window.as_ref() else {
            return Ok(());
        };
        let total = (self.lines.len() - lines) + (self.markers.len() - markers);
        self.counters.step(total)?;
        let kept = |line: &Line| meets(window, line.y, line.height);
        let mut dropped = 0u32;
        let mut seen = lines;
        let mut write = markers;
        for read in markers..self.markers.len() {
            let mut marker = *self.markers.get(read).ok_or(Error::Invalid)?;
            let keep = match marker.line {
                Some(index) if index as usize >= lines => {
                    let index = index as usize;
                    while seen < index {
                        if !self.lines.get(seen).is_some_and(kept) {
                            dropped += 1;
                        }
                        seen += 1;
                    }
                    marker.line = Some(index as u32 - dropped);
                    self.lines.get(index).is_some_and(kept)
                }
                Some(_) => true,
                None => meets(window, marker.y, marker.height),
            };
            if keep {
                *self.markers.get_mut(write).ok_or(Error::Invalid)? = marker;
                write += 1;
            } else {
                self.counters.fragments = self.counters.fragments.saturating_sub(1);
            }
        }
        self.markers.truncate(write);
        let mut write = lines;
        for read in lines..self.lines.len() {
            let line = self.lines.get(read).ok_or(Error::Invalid)?;
            if kept(line) {
                self.lines.swap(write, read);
                write += 1;
            } else {
                let gone = line.fragments.len() + line.replaced.len();
                self.counters.fragments = self.counters.fragments.saturating_sub(gone);
            }
        }
        self.lines.truncate(write);
        right_size(&mut self.counters, self.memory, &mut self.lines)?;
        right_size(&mut self.counters, self.memory, &mut self.markers)?;
        self.filtered = (self.lines.len(), self.markers.len());
        Ok(())
    }
}
/// Moves what a window kept into a vector its size, charging it and
/// giving back the old one's capacity charge, where the old is four times
/// too large: a window far into a long row otherwise holds the capacity
/// every line above it needed. A layout's vectors grow only through
/// Counters::admit_growth, which charged at least that.
fn right_size<T>(
    counters: &mut Counters,
    memory: &Budget,
    values: &mut BudgetVec<T>,
) -> Result<(), Error> {
    let (len, capacity) = (values.len(), values.capacity());
    if capacity <= 64 || capacity / 4 <= len {
        return Ok(());
    }
    let size = std::mem::size_of::<T>();
    let charge = len
        .checked_mul(size)
        .and_then(|n| n.checked_mul(2))
        .and_then(|n| n.checked_add(32))
        .ok_or(Error::Limit("MAX_PAGE_BYTES"))?;
    counters.charge(charge)?;
    counters.step(len)?;
    let mut fresh = td_alloc::BudgetVec::with_capacity(memory, len)?;
    while let Some(value) = values.pop() {
        fresh.try_push(value)?;
    }
    fresh.reverse();
    *values = fresh;
    counters.bytes = counters
        .bytes
        .saturating_sub(capacity.saturating_mul(size).saturating_mul(2));
    Ok(())
}
/// What a cell lays: the siblings from `first` to `until` in `strut`, and
/// its own box, None for an anonymous cell.
struct Inside {
    first: Option<BoxId>,
    until: Option<BoxId>,
    strut: StyleId,
    node: Option<BoxId>,
}
/// A table's laying: its grid and the columns' place.
struct Context {
    grid: Grid,
    content_x: f64,
    content_width: f64,
    down: f64,
    collapse: bool,
    offset: (f64, f64),
}
/// A cell's padding and border; collapsed, half its border (CSS 2.2
/// §17.6.2, each shared border split between the two cells beside it).
/// Percentages of padding are zero, measured and laid alike, so content
/// is laid at the width it was measured for.
fn cell_edges(style: &ComputedStyle, collapse: bool) -> (Edges, Edges) {
    let padding = edges(style, PADDING, 0.0, true);
    let border = edges(style, BORDER, 0.0, true);
    (padding, if collapse { half(border) } else { border })
}
/// Raises a span of columns' `field` to hold `need` together, in shares of
/// their maxima, or evenly where those are all zero (§17.5.2.2 step 2).
fn spread(cols: &mut [Col], need: i64, field: impl Fn(&mut Col) -> &mut i64) {
    let have: i64 = cols.iter_mut().map(|c| *field(c)).sum();
    if need <= have || cols.is_empty() {
        return;
    }
    let extra = need - have;
    let weights: i64 = cols.iter().map(|c| c.max).sum();
    let count = cols.len() as i64;
    let mut given = 0;
    let last = cols.len() - 1;
    for (at, col) in cols.iter_mut().enumerate() {
        let part = if at == last {
            extra - given
        } else if weights > 0 {
            (i128::from(extra) * i128::from(col.max) / i128::from(weights)) as i64
        } else {
            extra / count
        };
        *field(col) += part;
        given += part;
    }
}
/// Adds `extra` to the columns of a class in shares of their `weight`,
/// or evenly where those are all zero; the last takes what rounding left.
fn give(cols: &mut [Col], extra: i64, class: fn(&Col) -> bool, weight: fn(&Col) -> i64) {
    let count = cols.iter().filter(|c| class(c)).count() as i64;
    let weights: i64 = cols.iter().filter(|c| class(c)).map(weight).sum();
    let last = cols.iter().rposition(class);
    let mut given = 0;
    for (at, col) in cols.iter_mut().enumerate() {
        if !class(col) {
            continue;
        }
        let part = if Some(at) == last {
            extra - given
        } else if weights > 0 {
            (i128::from(extra) * i128::from(weight(col)) / i128::from(weights)) as i64
        } else {
            extra / count.max(1)
        };
        col.width += part;
        given += part;
    }
}
/// Takes `take`, at most `slack`, from the columns of a class in shares of
/// their room above their minima, `slack` in all, then what the shares'
/// rounding left from those with room, so the columns fill the table.
fn take_from(cols: &mut [Col], take: i64, slack: i64, class: fn(&Col) -> bool) {
    let mut left = take;
    for col in cols.iter_mut().filter(|c| class(c) && c.width > c.min) {
        let part = (i128::from(take) * i128::from(col.width - col.min) / i128::from(slack)) as i64;
        let part = part.min(left);
        col.width -= part;
        left -= part;
    }
    for col in cols.iter_mut().filter(|c| class(c)) {
        if left <= 0 {
            break;
        }
        let part = left.min(col.width - col.min).max(0);
        col.width -= part;
        left -= part;
    }
}

#[cfg(test)]
mod tests;
