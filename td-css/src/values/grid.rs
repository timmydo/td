//! CSS Grid 2's longhands and shorthands, without subgrid or masonry
//! (DESIGN.md §4, "Grid").
use super::*;
use Property::*;

/// Explicit tracks in one track list, integer `repeat()`s expanded and an
/// automatic repeat's tracks counted once; line names in one, as written;
/// and the rows and the columns of one `grid-template-areas`.
pub const MAX_GRID_TRACKS: usize = 1_000;
/// Named areas in one `grid-template-areas`.
pub const MAX_GRID_AREAS: usize = 1_000;
/// The magnitude grid line numbers and spans clamp to, as Gecko's.
pub const MAX_GRID_LINE: i32 = 10_000;

/// A track breadth: a nonnegative length or percentage, a flex factor in
/// fr, or a keyword.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Breadth<L = Length> {
    Length(L),
    Flex(f64),
    Auto,
    MinContent,
    MaxContent,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TrackSize<L = Length> {
    Breadth(Breadth<L>),
    /// `minmax(min, max)`; the minimum is never a flex factor.
    MinMax(Breadth<L>, Breadth<L>),
    FitContent(L),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    /// At least 1; the list's expansion is at most MAX_GRID_TRACKS.
    Count(u32),
    AutoFill,
    AutoFit,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TrackItem<L = Length> {
    /// A `[...]` list: `TrackList::names()[start..start + len]`.
    Names {
        start: usize,
        len: usize,
    },
    Size(TrackSize<L>),
    /// `repeat()`, whose body is the `len` items after it.
    Repeat {
        repeat: Repeat,
        len: usize,
    },
}
/// A track list as written, `repeat()` kept compact. Adjacent name lists
/// are one item; a list holds at most one automatic repeat.
#[derive(Debug, PartialEq)]
pub struct TrackList<L = Length> {
    pub(crate) items: BudgetVec<TrackItem<L>>,
    pub(crate) names: BudgetShared<BudgetVec<Text>>,
}
impl<L> TrackList<L> {
    pub fn items(&self) -> &[TrackItem<L>] {
        &self.items
    }
    pub fn names(&self) -> &[Text] {
        &self.names
    }
    /// The names of a `Names` item.
    pub fn line_names(&self, start: usize, len: usize) -> Option<&[Text]> {
        self.names.get(start..start.checked_add(len)?)
    }
}
impl TrackList {
    /// Equal to the bit, as `ComputedValue::same`.
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.items.len() == other.items.len()
            && *self.names == *other.names
            && self
                .items
                .iter()
                .zip(other.items.iter())
                .all(|(a, b)| match (a, b) {
                    (TrackItem::Size(a), TrackItem::Size(b)) => size_same(a, b),
                    _ => a == b,
                })
    }
}
fn length_same(a: &Length, b: &Length) -> bool {
    ComputedValue::Length(*a).same(&ComputedValue::Length(*b))
}
fn breadth_same(a: &Breadth, b: &Breadth) -> bool {
    match (a, b) {
        (Breadth::Length(a), Breadth::Length(b)) => length_same(a, b),
        (Breadth::Flex(a), Breadth::Flex(b)) => a.to_bits() == b.to_bits(),
        _ => a == b,
    }
}
fn size_same(a: &TrackSize, b: &TrackSize) -> bool {
    match (a, b) {
        (TrackSize::Breadth(a), TrackSize::Breadth(b)) => breadth_same(a, b),
        (TrackSize::MinMax(a, b), TrackSize::MinMax(c, d)) => {
            breadth_same(a, c) && breadth_same(b, d)
        }
        (TrackSize::FitContent(a), TrackSize::FitContent(b)) => length_same(a, b),
        _ => false,
    }
}

/// One named area: zero-based rows and columns, each end exclusive.
#[derive(Debug, PartialEq, Eq)]
pub struct GridArea {
    pub name: Text,
    pub row_start: usize,
    pub row_end: usize,
    pub column_start: usize,
    pub column_end: usize,
}
/// `grid-template-areas` computed: its rows and columns and its named
/// areas, sorted by name.
#[derive(Debug, PartialEq, Eq)]
pub struct GridAreas {
    pub(crate) rows: usize,
    pub(crate) columns: usize,
    pub(crate) areas: BudgetVec<GridArea>,
}
impl GridAreas {
    pub fn rows(&self) -> usize {
        self.rows
    }
    pub fn columns(&self) -> usize {
        self.columns
    }
    pub fn areas(&self) -> &[GridArea] {
        &self.areas
    }
}
/// A grid-placement longhand other than `auto`.
#[derive(Debug, PartialEq, Eq)]
pub struct GridLine {
    pub span: bool,
    /// Clamped to ±MAX_GRID_LINE: never 0 but for a lone name; a span
    /// without one is 1.
    pub integer: i32,
    pub name: Option<Text>,
}

/// A track list's three grammars.
#[derive(Clone, Copy, PartialEq)]
enum Lists {
    /// `grid-template-rows` and `-columns`: names, sizes and repeats.
    Template,
    /// `<explicit-track-list>`: names and sizes.
    Explicit,
    /// `grid-auto-rows` and `-columns`: sizes alone.
    Auto,
}
struct Built {
    items: BudgetVec<TrackItem<Axis>>,
    names: BudgetVec<Text>,
    /// Explicit tracks, repeats expanded.
    tracks: usize,
    /// Whether a length needs the element's font sizes.
    relative: bool,
    auto_repeat: bool,
    /// Whether a size outside the automatic repeat is not fixed.
    unfixed: bool,
}
impl Built {
    fn new(memory: &Budget) -> Result<Self, Error> {
        Ok(Self {
            items: BudgetVec::new(memory)?,
            names: BudgetVec::new(memory)?,
            tracks: 0,
            relative: false,
            auto_repeat: false,
            unfixed: false,
        })
    }
    /// A `[...]` list's names, or false if one is no custom-ident or
    /// they pass MAX_GRID_TRACKS. `merge` joins them to the names item
    /// just before.
    fn names(&mut self, values: &[Component], merge: bool) -> Result<bool, Error> {
        let start = self.names.len();
        for c in values.iter().filter(|c| !c.whitespace()) {
            let Some(name) = c.ident_owned().filter(|n| custom_ident(n)) else {
                return Ok(false);
            };
            if self.names.len() == MAX_GRID_TRACKS {
                return Ok(false);
            }
            self.names.try_push(name.try_clone()?)?;
        }
        let added = self.names.len() - start;
        if merge {
            if let Some(TrackItem::Names { len, .. }) = self.items.last_mut() {
                *len += added;
                return Ok(true);
            }
        }
        self.items
            .try_push(TrackItem::Names { start, len: added })?;
        Ok(true)
    }
    /// Count `tracks` more, false past MAX_GRID_TRACKS.
    fn count(&mut self, tracks: usize) -> bool {
        match self.tracks.checked_add(tracks) {
            Some(n) if n <= MAX_GRID_TRACKS => {
                self.tracks = n;
                true
            }
            _ => false,
        }
    }
    fn size(&mut self, size: TrackSize<Axis>) -> Result<bool, Error> {
        self.relative |= relative(&size);
        self.unfixed |= !fixed(&size);
        self.items.try_push(TrackItem::Size(size))?;
        Ok(self.count(1))
    }
    /// `repeat()`'s count and body; false if invalid.
    fn repetition(&mut self, args: &[Component]) -> Result<bool, Error> {
        let mut parts = args.split(|c| c.delim(','));
        let (Some(count), Some(body), None) = (parts.next(), parts.next(), parts.next()) else {
            return Ok(false);
        };
        let repeat = match syntax::trim(count) {
            [Component::Token(Token::Number(n))] if n.integer => match number(&n.representation) {
                Some(n) if (1.0..=MAX_GRID_TRACKS as f64).contains(&n) => Repeat::Count(n as u32),
                _ => return Ok(false),
            },
            [c] => match c
                .ident()
                .and_then(|n| syntax::keyword(n, &["auto-fill", "auto-fit"]))
            {
                Some("auto-fill") => Repeat::AutoFill,
                Some("auto-fit") => Repeat::AutoFit,
                _ => return Ok(false),
            },
            _ => return Ok(false),
        };
        let automatic = !matches!(repeat, Repeat::Count(_));
        if automatic && std::mem::replace(&mut self.auto_repeat, true) {
            return Ok(false);
        }
        let at = self.items.len();
        self.items.try_push(TrackItem::Repeat { repeat, len: 0 })?;
        let (mut sizes, mut after_names) = (0usize, false);
        for c in body.iter().filter(|c| !c.whitespace()) {
            if let Component::Block { kind: '[', values } = c {
                if std::mem::replace(&mut after_names, true) || !self.names(values, false)? {
                    return Ok(false);
                }
                continue;
            }
            after_names = false;
            let Some(size) = track_size(c)? else {
                return Ok(false);
            };
            if automatic && !fixed(&size) {
                return Ok(false);
            }
            self.relative |= relative(&size);
            self.unfixed |= !automatic && !fixed(&size);
            self.items.try_push(TrackItem::Size(size))?;
            sizes += 1;
        }
        let len = self.items.len() - at - 1;
        if let Some(TrackItem::Repeat { len: slot, .. }) = self.items.get_mut(at) {
            *slot = len;
        }
        let times = match repeat {
            Repeat::Count(n) => n as usize,
            _ => 1,
        };
        Ok(sizes > 0 && sizes.checked_mul(times).is_some_and(|n| self.count(n)))
    }
    fn finish(self, memory: &Budget) -> Result<TrackList<Axis>, Error> {
        Ok(TrackList {
            items: self.items,
            names: BudgetShared::new(memory, self.names)?,
        })
    }
}
/// A custom-ident a line name may be: no CSS-wide keyword, `default`,
/// `span` or `auto`.
fn custom_ident(name: &str) -> bool {
    syntax::keyword(
        name,
        &[
            "initial",
            "inherit",
            "unset",
            "revert",
            "revert-layer",
            "default",
            "span",
            "auto",
        ],
    )
    .is_none()
}
fn relative(size: &TrackSize<Axis>) -> bool {
    let axis = |a: &Axis| match a {
        Axis::Length(_, unit) => matches!(unit, Unit::Em | Unit::Rem),
        Axis::Calculated(value) => value.font_relative(),
    };
    let breadth = |b: &Breadth<Axis>| matches!(b, Breadth::Length(a) if axis(a));
    match size {
        TrackSize::Breadth(b) => breadth(b),
        TrackSize::MinMax(a, b) => breadth(a) || breadth(b),
        TrackSize::FitContent(a) => axis(a),
    }
}
/// `<fixed-size>`: a length or percentage, or a minmax() with one.
fn fixed(size: &TrackSize<Axis>) -> bool {
    match size {
        TrackSize::Breadth(b) => matches!(b, Breadth::Length(_)),
        TrackSize::MinMax(a, b) => {
            matches!(a, Breadth::Length(_)) || matches!(b, Breadth::Length(_))
        }
        TrackSize::FitContent(_) => false,
    }
}
/// A nonnegative length or percentage, calc() included.
fn length_percentage(c: &Component) -> Result<Option<Axis>, Error> {
    Ok(match parse_length(GridAutoRows, std::slice::from_ref(c))? {
        Some(Specified::Length(n, unit)) if !matches!(unit, Unit::Auto) => {
            Some(Axis::Length(n, unit))
        }
        Some(Specified::Calculated(value)) => Some(Axis::Calculated(value)),
        _ => None,
    })
}
fn breadth(c: &Component, flex: bool) -> Result<Option<Breadth<Axis>>, Error> {
    if let Some(name) = c.ident() {
        return Ok(
            match syntax::keyword(name, &["auto", "min-content", "max-content"]) {
                Some("auto") => Some(Breadth::Auto),
                Some("min-content") => Some(Breadth::MinContent),
                Some("max-content") => Some(Breadth::MaxContent),
                _ => None,
            },
        );
    }
    if let Component::Token(Token::Dimension(n, unit)) = c {
        if unit.eq_ignore_ascii_case("fr") {
            return Ok(number(&n.representation)
                .filter(|n| flex && *n >= 0.0)
                .map(|n| Breadth::Flex(n.min(MAX_ABSOLUTE_VALUE))));
        }
    }
    Ok(length_percentage(c)?.map(Breadth::Length))
}
fn lone(part: &[Component]) -> Option<&Component> {
    match syntax::trim(part) {
        [c] => Some(c),
        _ => None,
    }
}
fn track_size(c: &Component) -> Result<Option<TrackSize<Axis>>, Error> {
    if let Component::Function { name, values } = c {
        if name.eq_ignore_ascii_case("minmax") {
            let mut args = values.split(|c| c.delim(','));
            let (Some(min), Some(max), None) = (
                args.next().and_then(lone),
                args.next().and_then(lone),
                args.next(),
            ) else {
                return Ok(None);
            };
            let (Some(min), Some(max)) = (breadth(min, false)?, breadth(max, true)?) else {
                return Ok(None);
            };
            return Ok(Some(TrackSize::MinMax(min, max)));
        }
        if name.eq_ignore_ascii_case("fit-content") {
            let Some(arg) = lone(values) else {
                return Ok(None);
            };
            return Ok(length_percentage(arg)?.map(TrackSize::FitContent));
        }
    }
    Ok(breadth(c, true)?.map(TrackSize::Breadth))
}
fn track_list(v: &[Component], lists: Lists, memory: &Budget) -> Result<Option<Built>, Error> {
    let mut built = Built::new(memory)?;
    let mut after_names = false;
    for c in v.iter().filter(|c| !c.whitespace()) {
        if let Component::Block { kind: '[', values } = c {
            if lists == Lists::Auto
                || std::mem::replace(&mut after_names, true)
                || !built.names(values, false)?
            {
                return Ok(None);
            }
            continue;
        }
        after_names = false;
        if let Component::Function { name, values } = c {
            if name.eq_ignore_ascii_case("repeat") {
                if lists != Lists::Template || !built.repetition(values)? {
                    return Ok(None);
                }
                continue;
            }
        }
        let Some(size) = track_size(c)? else {
            return Ok(None);
        };
        if !built.size(size)? {
            return Ok(None);
        }
    }
    // An automatic repeat takes no intrinsic or flexible track beside it.
    if built.tracks == 0 || (built.auto_repeat && built.unfixed) {
        return Ok(None);
    }
    Ok(Some(built))
}
/// A built list as its value: computed now when no length needs the
/// element's font sizes, else per element.
fn tracks(built: Built, memory: &Budget) -> Result<Specified, Error> {
    let relative = built.relative;
    let list = built.finish(memory)?;
    Ok(if relative {
        Specified::Tracks(BudgetShared::new(memory, list)?)
    } else {
        Specified::Ready(ComputedValue::GridTracks(BudgetShared::new(
            memory,
            compute(&list, 0.0, 0.0, memory)?,
        )?))
    })
}
/// `none` or a track list.
fn template_axis(v: &[Component], memory: &Budget) -> Result<Option<Specified>, Error> {
    let v = syntax::trim(v);
    if single_ident(v).is_some_and(|n| n.eq_ignore_ascii_case("none")) {
        return Ok(Some(Specified::Ready(ComputedValue::Keyword("none"))));
    }
    match track_list(v, Lists::Template, memory)? {
        Some(built) => Ok(Some(tracks(built, memory)?)),
        None => Ok(None),
    }
}
/// `grid-auto-rows` and `-columns`: `auto`, the initial value, or sizes.
fn auto_tracks(v: &[Component], memory: &Budget) -> Result<Option<Specified>, Error> {
    let v = syntax::trim(v);
    if single_ident(v).is_some_and(|n| n.eq_ignore_ascii_case("auto")) {
        return Ok(Some(Specified::Ready(ComputedValue::Keyword("auto"))));
    }
    match track_list(v, Lists::Auto, memory)? {
        Some(built) => Ok(Some(tracks(built, memory)?)),
        None => Ok(None),
    }
}
/// A specified track list computed: lengths in px at the element's font
/// sizes, percentages kept, as flex-basis's.
pub(super) fn compute(
    list: &TrackList<Axis>,
    em: f64,
    rem: f64,
    memory: &Budget,
) -> Result<TrackList, Error> {
    let length = |a: &Axis| match a {
        Axis::Length(n, Unit::Percent) => {
            Length::Percent(n.clamp(-MAX_ABSOLUTE_VALUE, MAX_ABSOLUTE_VALUE))
        }
        Axis::Length(n, unit) => {
            Length::Px((n * unit_px(*unit, em, rem)).clamp(-MAX_ABSOLUTE_VALUE, MAX_ABSOLUTE_VALUE))
        }
        Axis::Calculated(value) => value.compute(em, rem, None, true),
    };
    let breadth = |b: &Breadth<Axis>| match b {
        Breadth::Length(a) => Breadth::Length(length(a)),
        Breadth::Flex(n) => Breadth::Flex(*n),
        Breadth::Auto => Breadth::Auto,
        Breadth::MinContent => Breadth::MinContent,
        Breadth::MaxContent => Breadth::MaxContent,
    };
    let mut items = td_alloc::BudgetVec::with_capacity(memory, list.items.len())?;
    for item in list.items.iter() {
        let item = match item {
            TrackItem::Names { start, len } => TrackItem::Names {
                start: *start,
                len: *len,
            },
            TrackItem::Repeat { repeat, len } => TrackItem::Repeat {
                repeat: *repeat,
                len: *len,
            },
            TrackItem::Size(size) => TrackItem::Size(match size {
                TrackSize::Breadth(b) => TrackSize::Breadth(breadth(b)),
                TrackSize::MinMax(a, b) => TrackSize::MinMax(breadth(a), breadth(b)),
                TrackSize::FitContent(a) => TrackSize::FitContent(length(a)),
            }),
        };
        items
            .push_within_capacity(item)
            .map_err(|_| Error::InvalidContext)?;
    }
    Ok(TrackList {
        items,
        names: list.names.try_clone()?,
    })
}

/// Where a named area's cells lie, and how many there are.
#[derive(Clone, Copy)]
struct Cells {
    rows: (usize, usize),
    columns: (usize, usize),
    count: usize,
}
impl Cells {
    fn widen(&mut self, other: &Cells) {
        self.rows = (self.rows.0.min(other.rows.0), self.rows.1.max(other.rows.1));
        self.columns = (
            self.columns.0.min(other.columns.0),
            self.columns.1.max(other.columns.1),
        );
        self.count = self.count.saturating_add(other.count);
    }
}
/// A code point of a named cell's name (Grid 2 §7.3.1).
fn area_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii()
}
/// The named cells an areas string holds, valid or not: its runs of
/// name code points.
pub(super) fn named_cells(text: &str) -> usize {
    let mut previous = false;
    let mut cells = 0usize;
    for c in text.chars() {
        let ident = area_ident(c);
        if ident && !previous {
            cells += 1;
        }
        previous = ident;
    }
    cells
}
/// Units of `ComputeStats::grid_items` a reparse charges for each area it
/// builds, beside a byte and a named cell: its name's allocation and its
/// share of the merges, measured near 40 ns.
pub(super) const AREA_ITEMS: usize = 2;
/// The cells a pending run holds before it merges into `found`: at least
/// this many, and at least as many as `found` holds, so merging stays
/// amortized O(log n) a cell.
const AREA_RUN: usize = 64;
/// `grid-template-areas`' strings, a row each.
///
/// A sorted map's insertion shifts its tail, quadratic in names arriving
/// in descending order, as a reparsed `var()` brings them for every
/// element. A cell whose name `found` holds updates it in place, found by
/// binary search; any other is appended to `pending`, which, sorted once,
/// merges into a new `found` when it grows past AREA_RUN or `found`.
struct AreaRows<'a> {
    /// Distinct names, sorted, at most MAX_GRID_AREAS.
    found: BudgetVec<(&'a str, Cells)>,
    /// A cell each, in arrival order, none of their names in `found`.
    pending: BudgetVec<(&'a str, Cells)>,
    rows: usize,
    columns: usize,
    memory: &'a Budget,
}
impl<'a> AreaRows<'a> {
    fn new(memory: &'a Budget) -> Result<Self, Error> {
        Ok(Self {
            found: BudgetVec::new(memory)?,
            pending: BudgetVec::new(memory)?,
            rows: 0,
            columns: 0,
            memory,
        })
    }
    /// One string's cells (Grid 2 §7.3.1), or false: a trash character,
    /// no cell, a count unlike the first row's, or a bound passed.
    fn row(&mut self, text: &'a str) -> Result<bool, Error> {
        if self.rows == MAX_GRID_TRACKS {
            return Ok(false);
        }
        let ident = area_ident;
        let mut rest = text;
        let mut column = 0usize;
        loop {
            rest = rest.trim_start_matches([' ', '\t', '\n']);
            let Some(first) = rest.chars().next() else {
                break;
            };
            let end = if first == '.' {
                rest.find(|c| c != '.')
            } else if ident(first) {
                rest.find(|c| !ident(c))
            } else {
                return Ok(false);
            };
            let Some((cell, tail)) = rest.split_at_checked(end.unwrap_or(rest.len())) else {
                return Ok(false);
            };
            rest = tail;
            if column == MAX_GRID_TRACKS {
                return Ok(false);
            }
            if first != '.' && !self.cell(cell, self.rows, column)? {
                return Ok(false);
            }
            column += 1;
        }
        if column == 0 || (self.rows > 0 && column != self.columns) {
            return Ok(false);
        }
        self.columns = column;
        self.rows += 1;
        Ok(true)
    }
    /// One named cell; false past MAX_GRID_AREAS names.
    fn cell(&mut self, name: &'a str, row: usize, column: usize) -> Result<bool, Error> {
        let cell = Cells {
            rows: (row, row),
            columns: (column, column),
            count: 1,
        };
        if let Ok(at) = self.found.binary_search_by(|(n, _)| (*n).cmp(name)) {
            if let Some((_, cells)) = self.found.get_mut(at) {
                cells.widen(&cell);
            }
            return Ok(true);
        }
        self.pending.try_push((name, cell))?;
        if self.pending.len() >= AREA_RUN.max(self.found.len()) {
            return self.merge();
        }
        Ok(true)
    }
    /// `pending` sorted and merged into `found`, equal names joined; false
    /// past MAX_GRID_AREAS names.
    fn merge(&mut self) -> Result<bool, Error> {
        if self.pending.is_empty() {
            return Ok(true);
        }
        self.pending.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let capacity = self.found.len() + self.pending.len();
        let mut merged: BudgetVec<(&str, Cells)> =
            td_alloc::BudgetVec::with_capacity(self.memory, capacity)?;
        let (mut old, mut new) = (self.found.iter().peekable(), self.pending.iter().peekable());
        loop {
            let next = match (old.peek(), new.peek()) {
                (Some(a), Some(b)) if a.0 < b.0 => old.next(),
                (Some(_), None) => old.next(),
                (_, Some(_)) => new.next(),
                (None, None) => break,
            };
            let Some(&(name, cells)) = next else {
                break;
            };
            match merged.last_mut() {
                Some((last, joined)) if *last == name => joined.widen(&cells),
                _ => {
                    if merged.len() == MAX_GRID_AREAS {
                        return Ok(false);
                    }
                    merged
                        .push_within_capacity((name, cells))
                        .map_err(|_| Error::InvalidContext)?;
                }
            }
        }
        self.found = merged;
        self.pending.truncate(0);
        Ok(true)
    }
    /// The areas, if each name's cells fill their bounding rectangle,
    /// checked for every name before any is allocated.
    fn finish(mut self, memory: &Budget) -> Result<Option<Specified>, Error> {
        if self.rows == 0 || !self.merge()? {
            return Ok(None);
        }
        for (_, cells) in self.found.iter() {
            let height = cells.rows.1 - cells.rows.0 + 1;
            let width = cells.columns.1 - cells.columns.0 + 1;
            if height.checked_mul(width) != Some(cells.count) {
                return Ok(None);
            }
        }
        let mut areas = td_alloc::BudgetVec::with_capacity(memory, self.found.len())?;
        for (name, cells) in self.found.iter() {
            areas
                .push_within_capacity(GridArea {
                    name: syntax::text(memory, name)?,
                    row_start: cells.rows.0,
                    row_end: cells.rows.1 + 1,
                    column_start: cells.columns.0,
                    column_end: cells.columns.1 + 1,
                })
                .map_err(|_| Error::InvalidContext)?;
        }
        Ok(Some(Specified::Ready(ComputedValue::GridAreas(
            BudgetShared::new(
                memory,
                GridAreas {
                    rows: self.rows,
                    columns: self.columns,
                    areas,
                },
            )?,
        ))))
    }
}
fn template_areas(v: &[Component], memory: &Budget) -> Result<Option<Specified>, Error> {
    if single_ident(v).is_some_and(|n| n.eq_ignore_ascii_case("none")) {
        return Ok(Some(Specified::Ready(ComputedValue::Keyword("none"))));
    }
    let mut rows = AreaRows::new(memory)?;
    for c in v.iter().filter(|c| !c.whitespace()) {
        let Component::Token(Token::String(text)) = c else {
            return Ok(None);
        };
        if !rows.row(text)? {
            return Ok(None);
        }
    }
    rows.finish(memory)
}

/// `grid-auto-flow`: `row` or `column`, and `dense`, each at most once.
fn auto_flow(v: &[Component]) -> Option<Specified> {
    let (mut axis, mut dense) = (None, false);
    for c in v.iter().filter(|c| !c.whitespace()) {
        match syntax::keyword(c.ident()?, &["row", "column", "dense"])? {
            "dense" if !dense => dense = true,
            word @ ("row" | "column") if axis.is_none() => axis = Some(word),
            _ => return None,
        }
    }
    if axis.is_none() && !dense {
        return None;
    }
    Some(flow(axis == Some("column"), dense))
}
fn flow(column: bool, dense: bool) -> Specified {
    Specified::Ready(ComputedValue::Keyword(match (column, dense) {
        (false, false) => "row",
        (false, true) => "row dense",
        (true, false) => "column",
        (true, true) => "column dense",
    }))
}

/// A `<grid-line>`, its integer clamped to ±MAX_GRID_LINE as parsed.
fn line(v: &[Component], memory: &Budget) -> Result<Option<Specified>, Error> {
    let v = syntax::trim(v);
    if single_ident(v).is_some_and(|n| n.eq_ignore_ascii_case("auto")) {
        return Ok(Some(Specified::Ready(ComputedValue::Keyword("auto"))));
    }
    let (mut span, mut integer, mut name) = (false, None, None);
    // `span` comes first or last (Grid 2 §8.3): nothing follows it
    // when something preceded it.
    let mut inner = false;
    for (count, c) in v.iter().filter(|c| !c.whitespace()).enumerate() {
        if count == 3 || (span && inner) {
            return Ok(None);
        }
        match c {
            Component::Token(Token::Number(n)) if n.integer && integer.is_none() => {
                // Any length: one past f64's range is infinite, then clamped.
                integer = n.representation.parse::<f64>().ok().filter(|n| !n.is_nan());
                if integer.is_none() {
                    return Ok(None);
                }
            }
            Component::Token(Token::Ident(t)) if t.eq_ignore_ascii_case("span") && !span => {
                span = true;
                inner = count > 0;
                continue;
            }
            Component::Token(Token::Ident(t)) if custom_ident(t) && name.is_none() => {
                name = Some(t);
            }
            _ => return Ok(None),
        }
    }
    let max = f64::from(MAX_GRID_LINE);
    let integer = match (span, integer) {
        (true, Some(n)) if n >= 1.0 => n.min(max),
        (true, None) if name.is_some() => 1.0,
        (false, Some(n)) if n != 0.0 => n.clamp(-max, max),
        (false, None) if name.is_some() => 0.0,
        _ => return Ok(None),
    };
    let name = match name {
        Some(t) => Some(t.try_clone()?),
        None => None,
    };
    let value = GridLine {
        span,
        integer: integer as i32,
        name,
    };
    Ok(Some(Specified::Ready(ComputedValue::GridLine(
        BudgetShared::new(memory, value)?,
    ))))
}

/// The longhands' grammars.
pub(super) fn parse(
    p: Property,
    v: &[Component],
    memory: &Budget,
) -> Result<Option<Specified>, Error> {
    Ok(match p {
        GridTemplateColumns | GridTemplateRows => template_axis(v, memory)?,
        GridTemplateAreas => template_areas(v, memory)?,
        GridAutoColumns | GridAutoRows => auto_tracks(v, memory)?,
        GridAutoFlow => auto_flow(v),
        _ => line(v, memory)?,
    })
}

type Parsed = Option<BudgetVec<(Property, Specified)>>;
fn output(pairs: &mut [(Property, Option<Specified>)], memory: &Budget) -> Result<Parsed, Error> {
    let mut output = td_alloc::BudgetVec::with_capacity(memory, pairs.len())?;
    for (p, value) in pairs.iter_mut() {
        output
            .push_within_capacity((*p, value.take().unwrap_or(Specified::Initial)))
            .map_err(|_| Error::InvalidContext)?;
    }
    Ok(Some(output))
}
/// `grid-template` (Grid 2 §7.4): `none`, `<rows> / <columns>`, or the
/// areas form, each row a string with an optional size and names, then
/// an optional `/ <explicit-track-list>`. Rows, columns and areas.
fn template(
    v: &[Component],
    memory: &Budget,
) -> Result<Option<(Specified, Specified, Specified)>, Error> {
    let none = || Specified::Ready(ComputedValue::Keyword("none"));
    if single_ident(v).is_some_and(|n| n.eq_ignore_ascii_case("none")) {
        return Ok(Some((none(), none(), none())));
    }
    let mut parts = v.split(|c| c.delim('/'));
    let (Some(first), second, None) = (parts.next(), parts.next(), parts.next()) else {
        return Ok(None);
    };
    if !first
        .iter()
        .any(|c| matches!(c, Component::Token(Token::String(_))))
    {
        let Some(second) = second else {
            return Ok(None);
        };
        let (Some(rows), Some(columns)) = (
            template_axis(first, memory)?,
            template_axis(second, memory)?,
        ) else {
            return Ok(None);
        };
        return Ok(Some((rows, columns, none())));
    }
    #[derive(PartialEq)]
    enum At {
        Start,
        Leading,
        Row,
        Sized,
        Trailing,
    }
    let mut areas = AreaRows::new(memory)?;
    let mut built = Built::new(memory)?;
    let (mut at, mut size) = (At::Start, 0usize);
    for c in first.iter().filter(|c| !c.whitespace()) {
        match c {
            Component::Block { kind: '[', values } => {
                let merge = at == At::Trailing;
                at = match at {
                    At::Start | At::Trailing => At::Leading,
                    At::Row | At::Sized => At::Trailing,
                    At::Leading => return Ok(None),
                };
                if !built.names(values, merge)? {
                    return Ok(None);
                }
            }
            Component::Token(Token::String(text)) => {
                if !areas.row(text)? {
                    return Ok(None);
                }
                size = built.items.len();
                if !built.size(TrackSize::Breadth(Breadth::Auto))? {
                    return Ok(None);
                }
                at = At::Row;
            }
            _ if at == At::Row => {
                let Some(value) = track_size(c)? else {
                    return Ok(None);
                };
                built.relative |= relative(&value);
                let Some(slot) = built.items.get_mut(size) else {
                    return Ok(None);
                };
                *slot = TrackItem::Size(value);
                at = At::Sized;
            }
            _ => return Ok(None),
        }
    }
    if at == At::Leading {
        return Ok(None);
    }
    let columns = match second {
        Some(second) => match track_list(second, Lists::Explicit, memory)? {
            Some(columns) => tracks(columns, memory)?,
            None => return Ok(None),
        },
        None => none(),
    };
    let Some(areas) = areas.finish(memory)? else {
        return Ok(None);
    };
    Ok(Some((tracks(built, memory)?, columns, areas)))
}
pub(super) fn template_shorthand(v: &[Component], memory: &Budget) -> Result<Parsed, Error> {
    let Some((rows, columns, areas)) = template(v, memory)? else {
        return Ok(None);
    };
    output(
        &mut [
            (GridTemplateRows, Some(rows)),
            (GridTemplateColumns, Some(columns)),
            (GridTemplateAreas, Some(areas)),
        ],
        memory,
    )
}
/// `[auto-flow && dense?] <tracks>?`: whether it is dense, and its
/// tracks, if `part` is that.
fn auto_flow_part(part: &[Component], memory: &Budget) -> Result<Option<(bool, Specified)>, Error> {
    let (mut keyword, mut dense, mut rest) = (false, false, part.len());
    for (i, c) in part.iter().enumerate().filter(|(_, c)| !c.whitespace()) {
        match c
            .ident()
            .and_then(|n| syntax::keyword(n, &["auto-flow", "dense"]))
        {
            Some("auto-flow") if !keyword => keyword = true,
            Some("dense") if !dense => dense = true,
            _ => {
                rest = i;
                break;
            }
        }
    }
    let tracks = part.get(rest..).unwrap_or_default();
    if !keyword {
        return Ok(None);
    }
    if tracks.iter().all(Component::whitespace) {
        return Ok(Some((dense, Specified::Initial)));
    }
    Ok(auto_tracks(tracks, memory)?.map(|tracks| (dense, tracks)))
}
/// `grid` (Grid 2 §7.8): a `grid-template`, or rows then `auto-flow`
/// columns, or `auto-flow` rows then columns; it resets the rest of the
/// six, never the gaps.
pub(super) fn grid(v: &[Component], memory: &Budget) -> Result<Parsed, Error> {
    let mut values = [
        (GridTemplateRows, None),
        (GridTemplateColumns, None),
        (GridTemplateAreas, None),
        (GridAutoRows, None),
        (GridAutoColumns, None),
        (GridAutoFlow, None),
    ];
    let set = |values: &mut [(Property, Option<Specified>)], p: Property, value: Specified| {
        if let Some((_, slot)) = values.iter_mut().find(|(q, _)| *q == p) {
            *slot = Some(value);
        }
    };
    if let Some((rows, columns, areas)) = template(v, memory)? {
        set(&mut values, GridTemplateRows, rows);
        set(&mut values, GridTemplateColumns, columns);
        set(&mut values, GridTemplateAreas, areas);
        return output(&mut values, memory);
    }
    let mut parts = v.split(|c| c.delim('/'));
    let (Some(left), Some(right), None) = (parts.next(), parts.next(), parts.next()) else {
        return Ok(None);
    };
    if let Some((dense, rows)) = auto_flow_part(left, memory)? {
        let Some(columns) = template_axis(right, memory)? else {
            return Ok(None);
        };
        set(&mut values, GridAutoRows, rows);
        set(&mut values, GridTemplateColumns, columns);
        set(&mut values, GridAutoFlow, flow(false, dense));
    } else if let Some((dense, columns)) = auto_flow_part(right, memory)? {
        let Some(rows) = template_axis(left, memory)? else {
            return Ok(None);
        };
        set(&mut values, GridAutoColumns, columns);
        set(&mut values, GridTemplateRows, rows);
        set(&mut values, GridAutoFlow, flow(true, dense));
    } else {
        return Ok(None);
    }
    output(&mut values, memory)
}
/// `grid-row`, `grid-column` (`targets` their start and end) and
/// `grid-area` (row start, column start, row end, column end): one to
/// two or four lines; an omitted one copies the line Grid 2 §8.4 names
/// when that is a lone custom-ident, and is `auto` otherwise.
pub(super) fn lines(
    targets: &[Property],
    v: &[Component],
    memory: &Budget,
) -> Result<Parsed, Error> {
    let mut parsed: [Option<Specified>; 4] = [None, None, None, None];
    for (count, part) in v.split(|c| c.delim('/')).enumerate() {
        let Some(slot) = parsed.get_mut(count).filter(|_| count < targets.len()) else {
            return Ok(None);
        };
        let Some(value) = line(part, memory)? else {
            return Ok(None);
        };
        *slot = Some(value);
    }
    // Each line's source when omitted: grid-area's column start and row
    // end copy the row start, its column end the column start; a
    // two-line shorthand's end copies its start.
    let sources: &[usize] = if targets.len() == 4 {
        &[0, 0, 0, 1]
    } else {
        &[0, 0]
    };
    let mut output: BudgetVec<(Property, Specified)> =
        td_alloc::BudgetVec::with_capacity(memory, targets.len())?;
    for ((p, source), given) in targets.iter().zip(sources).zip(parsed.iter_mut()) {
        let value = match given.take() {
            Some(value) => value,
            None => match output.get(*source) {
                Some((_, value)) if copies(value) => value.try_clone()?,
                _ => Specified::Ready(ComputedValue::Keyword("auto")),
            },
        };
        output
            .push_within_capacity((*p, value))
            .map_err(|_| Error::InvalidContext)?;
    }
    Ok(Some(output))
}
/// Whether a line is a lone custom-ident, which an omitted line copies.
fn copies(value: &Specified) -> bool {
    matches!(value, Specified::Ready(ComputedValue::GridLine(line))
        if !line.span && line.integer == 0 && line.name.is_some())
}

#[cfg(test)]
mod tests;
