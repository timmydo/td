//! A directory finder over a listing the consumer supplies: the folder's
//! path on a chrome row, a filter `TextEntry`, the entries as a `List`
//! and a status row, inside one rectangle on a surface. The consumer
//! reads the filesystem under its own bounds and trust and hands the
//! widget one `Listing` per folder; the widget owns the filter, the
//! selection and the scroll window, and answers navigation with typed
//! outcomes the consumer carries out: descend into an entry, ascend to
//! the parent, choose an entry or the listed folder, or cancel. Nothing
//! here reads a file, a clock or the environment.

use crate::chrome::{Field, Item, List, TextEntry, ROW};
use crate::filter;
use crate::raster::{
    text_run, Draw, GlyphStyle, Primitive, Rect, Surface, BORDER, CHROME, INK, LINE_NUMBER,
};
use crate::CELL_WIDTH;

/// The entries a listing may hold, and the bytes of names and metas
/// between them.
pub const ENTRIES: usize = 4096;
pub const LISTING_BYTES: usize = 1024 * 1024;
/// One entry's name and right-aligned meta, one listing's path and the
/// status note, in bytes; the filter query's bound is the launcher's.
pub const NAME_BYTES: usize = 1024;
pub const META_BYTES: usize = 16;
pub const PATH_BYTES: usize = 4096;
pub const QUERY_BYTES: usize = filter::MAX_QUERY_BYTES;
pub const NOTE_BYTES: usize = 256;
/// The label cells a row keeps beside its meta: a meta the row cannot
/// hold with that many is not shown.
pub const LABEL_COLUMNS: usize = 8;
/// The text cells a finder needs across, so the path, the filter and a
/// row beside the list's gutter each show something.
pub const MIN_COLUMNS: usize = 20;

/// The text inset inside a chrome row, the row painter's.
const INSET: (i64, i64) = (8, 4);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Limit,
    InvalidText,
    InvalidSurface,
    NoRoom,
    Allocation,
    NoEntry,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Limit => "finder limit exceeded",
            Self::InvalidText => "invalid finder text",
            Self::InvalidSurface => "invalid finder surface",
            Self::NoRoom => "surface cannot show the finder",
            Self::Allocation => "finder allocation failed",
            Self::NoEntry => "no such finder entry",
        })
    }
}
impl std::error::Error for Error {}

fn copy_text(text: &str, limit: usize) -> Result<String, Error> {
    if text.len() > limit {
        return Err(Error::Limit);
    }
    if text.chars().any(char::is_control) {
        return Err(Error::InvalidText);
    }
    let mut owned = String::new();
    owned
        .try_reserve_exact(text.len())
        .map_err(|_| Error::Allocation)?;
    owned.push_str(text);
    Ok(owned)
}

/// What an entry is: a folder the finder can descend into, or a file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Folder,
    File,
}

/// One listed entry: its name as the consumer wants it shown, a
/// right-aligned meta (`""` for none; a folder without one shows
/// `folder`), its kind, whether it can be descended into or chosen, and
/// whether it carries the mark, the list's star prefix, whose meaning
/// (a multiple selection) is the consumer's.
#[derive(Debug)]
pub struct Entry {
    name: String,
    meta: String,
    kind: Kind,
    enabled: bool,
    marked: bool,
}
impl Entry {
    pub fn new(name: &str, meta: &str, kind: Kind, enabled: bool) -> Result<Self, Error> {
        if name.is_empty() {
            return Err(Error::InvalidText);
        }
        Ok(Self {
            name: copy_text(name, NAME_BYTES)?,
            meta: copy_text(meta, META_BYTES)?,
            kind,
            enabled,
            marked: false,
        })
    }
    /// The entry with its mark set or cleared, for a listing built with
    /// marks the consumer remembers.
    pub fn with_marked(mut self, marked: bool) -> Self {
        self.marked = marked;
        self
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn meta(&self) -> &str {
        &self.meta
    }
    pub fn kind(&self) -> Kind {
        self.kind
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn marked(&self) -> bool {
        self.marked
    }
    /// The text bytes, for the listing bound.
    fn text_bytes(&self) -> usize {
        self.name.len() + self.meta.len()
    }
    /// The retained capacity, for storage accounting.
    fn bytes(&self) -> usize {
        self.name.capacity() + self.meta.capacity()
    }
}

/// One folder as the consumer read it: its path as shown, its entries in
/// the consumer's order, and whether the read stopped short of the
/// folder's end.
#[derive(Debug)]
pub struct Listing {
    path: String,
    entries: Vec<Entry>,
    truncated: bool,
}
impl Listing {
    pub fn new(path: &str, entries: Vec<Entry>, truncated: bool) -> Result<Self, Error> {
        if path.is_empty() {
            return Err(Error::InvalidText);
        }
        let path = copy_text(path, PATH_BYTES)?;
        if entries.len() > ENTRIES {
            return Err(Error::Limit);
        }
        let bytes = entries
            .iter()
            .try_fold(0usize, |total, entry| total.checked_add(entry.text_bytes()))
            .ok_or(Error::Limit)?;
        if bytes > LISTING_BYTES {
            return Err(Error::Limit);
        }
        Ok(Self {
            path,
            entries,
            truncated,
        })
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
    pub fn truncated(&self) -> bool {
        self.truncated
    }
    pub fn storage_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.path.capacity()
            + self.entries.capacity() * std::mem::size_of::<Entry>()
            + self.entries.iter().map(Entry::bytes).sum::<usize>()
    }
}

/// What the finder is for: a folder, chosen with `Accept` as the one
/// listed, or a file, chosen with `Activate` or `Accept` on it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Choose {
    Folder,
    File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// Return: descend into the selected folder, or choose the selected
    /// file when files are what is chosen.
    Activate,
    /// Choose the listed folder, or the selected file.
    Accept,
    /// Ascend to the parent.
    Parent,
    /// Delete the last filter character, or with no filter ascend.
    Backspace,
    Escape,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Key {
        key: Key,
        repeated: bool,
    },
    /// A typed character for the filter.
    Insert(char),
    Press {
        x: i64,
        y: i64,
    },
    Release {
        x: i64,
        y: i64,
    },
    Move {
        x: i64,
        y: i64,
    },
    Wheel {
        x: i64,
        y: i64,
        rows: isize,
    },
    Other,
    Resize {
        surface: Surface,
        rect: Rect,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Choice {
    /// The entry at this index of the listing.
    Entry(usize),
    /// The listed folder itself.
    Here,
    Cancelled,
    Unavailable(Error),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Ignored,
    Consumed,
    Changed,
    /// The consumer lists the folder at this index and installs it with
    /// `set_listing`, or notes why it could not with `set_note`.
    Descend(usize),
    /// The consumer lists the parent and installs it, selecting the folder
    /// it came from by name.
    Ascend,
    Closed(Choice),
}

/// The finder's controller: the listing, the filter query, the shown
/// indices, the selection and the scroll window, over a path row, a
/// filter field, the list and a status row laid out inside `rect`.
#[derive(Debug)]
pub struct Controller {
    listing: Listing,
    choose: Choose,
    surface: Surface,
    rect: Rect,
    filter: TextEntry,
    list: List,
    query: String,
    shown: Vec<usize>,
    selected: usize,
    first: usize,
    note: String,
    open: bool,
}

impl Controller {
    /// The finder over `listing` inside `rect`, the selection on the entry
    /// named `select` when it is listed (the folder the consumer came from),
    /// else the first.
    pub fn new(
        listing: Listing,
        choose: Choose,
        surface: Surface,
        rect: Rect,
        select: Option<&str>,
    ) -> Result<Self, Error> {
        let (filter, list) = Self::layout(surface, rect)?;
        let mut query = String::new();
        query
            .try_reserve_exact(QUERY_BYTES)
            .map_err(|_| Error::Allocation)?;
        let mut note = String::new();
        note.try_reserve_exact(NOTE_BYTES)
            .map_err(|_| Error::Allocation)?;
        let mut shown = Vec::new();
        shown
            .try_reserve_exact(listing.entries.len())
            .map_err(|_| Error::Allocation)?;
        let mut finder = Self {
            listing,
            choose,
            surface,
            rect,
            filter,
            list,
            query,
            shown,
            selected: 0,
            first: 0,
            note,
            open: true,
        };
        finder.refresh(select);
        Ok(finder)
    }

    /// The path row, the filter row and the status row are one `ROW` each;
    /// the list takes the whole rows between, and any remainder above the
    /// status row is chrome.
    fn layout(surface: Surface, rect: Rect) -> Result<(TextEntry, List), Error> {
        surface.check().map_err(|_| Error::InvalidSurface)?;
        if rect.intersection(surface.bounds()) != Some(rect) {
            return Err(Error::NoRoom);
        }
        let scale = surface.scale.value();
        let row = ROW * scale;
        if (rect.height as usize) < 4 * row
            || rect.width as usize / (CELL_WIDTH * scale) < MIN_COLUMNS
        {
            return Err(Error::NoRoom);
        }
        let filter = TextEntry::new(
            surface,
            Rect {
                y: rect.y + row as i64,
                height: row as u32,
                ..rect
            },
        )
        .ok_or(Error::NoRoom)?;
        let rows = (rect.height as usize - 3 * row) / row;
        let list = List::new(
            surface,
            Rect {
                y: rect.y + (2 * row) as i64,
                height: (rows * row) as u32,
                ..rect
            },
        )
        .ok_or(Error::NoRoom)?;
        Ok((filter, list))
    }

    fn row_height(&self) -> i64 {
        (ROW * self.surface.scale.value()) as i64
    }
    pub fn rect(&self) -> Rect {
        self.rect
    }
    pub fn path_rect(&self) -> Rect {
        Rect {
            height: self.row_height() as u32,
            ..self.rect
        }
    }
    pub fn filter_rect(&self) -> Rect {
        self.filter.rect()
    }
    pub fn list_rect(&self) -> Rect {
        self.list.rect()
    }
    pub fn status_rect(&self) -> Rect {
        let height = self.row_height();
        Rect {
            y: self.rect.y + i64::from(self.rect.height) - height,
            height: height as u32,
            ..self.rect
        }
    }
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn listing(&self) -> &Listing {
        &self.listing
    }
    pub fn query(&self) -> &str {
        &self.query
    }
    pub fn note(&self) -> &str {
        &self.note
    }
    /// The entry indices the filter admits, in listing order.
    pub fn shown(&self) -> &[usize] {
        &self.shown
    }
    /// The first shown row's position among the shown.
    pub fn first(&self) -> usize {
        self.first
    }
    /// The selected entry's index into the listing, when any is shown.
    pub fn selected(&self) -> Option<usize> {
        self.shown.get(self.selected).copied()
    }
    pub fn selected_entry(&self) -> Option<&Entry> {
        self.listing.entries.get(self.selected()?)
    }
    /// Retained listing, query, note and index capacities, excluding
    /// allocator bookkeeping.
    pub fn storage_bytes(&self) -> usize {
        std::mem::size_of::<Self>() - std::mem::size_of::<Listing>()
            + self.listing.storage_bytes()
            + self.query.capacity()
            + self.note.capacity()
            + self.shown.capacity() * std::mem::size_of::<usize>()
    }

    /// Installs a folder's listing: the filter and the note clear, and
    /// the selection lands on the entry named `select` when it is listed
    /// (the folder an ascent came from), else the first.
    pub fn set_listing(&mut self, listing: Listing, select: Option<&str>) -> Result<(), Error> {
        // The room is found before anything changes, so a refusal leaves
        // the finder showing the listing it had.
        let mut shown = Vec::new();
        shown
            .try_reserve_exact(listing.entries.len())
            .map_err(|_| Error::Allocation)?;
        self.shown = shown;
        self.listing = listing;
        self.query.clear();
        self.note.clear();
        self.refresh(select);
        Ok(())
    }

    /// Sets or clears the mark of the entry at `index` in the listing (not
    /// among the shown), `NoEntry` when there is none; the frame changes,
    /// nothing else does.
    pub fn set_marked(&mut self, index: usize, marked: bool) -> Result<(), Error> {
        let entry = self.listing.entries.get_mut(index).ok_or(Error::NoEntry)?;
        entry.marked = marked;
        Ok(())
    }

    /// Sets the status row's note, shown until the next listing: what the
    /// consumer has to say about the last descent or ascent it refused.
    pub fn set_note(&mut self, note: &str) -> Result<(), Error> {
        if note.len() > NOTE_BYTES {
            return Err(Error::Limit);
        }
        if note.chars().any(char::is_control) {
            return Err(Error::InvalidText);
        }
        // Within the `NOTE_BYTES` reserved at construction, which nothing
        // shrinks, so the push allocates nothing; the query's `insert`
        // holds to `QUERY_BYTES` the same way.
        self.note.clear();
        self.note.push_str(note);
        Ok(())
    }

    /// Recomputes the shown indices from the query and puts the selection
    /// on `select`'s entry when shown, else the first shown.
    fn refresh(&mut self, select: Option<&str>) {
        // Within the room `new` or `set_listing` reserved for every entry,
        // so the pushes allocate nothing.
        self.shown.clear();
        for (index, entry) in self.listing.entries.iter().enumerate() {
            if matches(&entry.name, &self.query) {
                self.shown.push(index);
            }
        }
        self.selected = select
            .and_then(|name| {
                self.shown.iter().position(|index| {
                    self.listing
                        .entries
                        .get(*index)
                        .is_some_and(|entry| entry.name == name)
                })
            })
            .unwrap_or(0);
        self.first = 0;
        self.reveal();
    }

    fn reveal(&mut self) {
        self.first = self
            .list
            .reveal(self.shown.len(), self.selected, self.first);
    }

    fn close(&mut self, choice: Choice) -> Outcome {
        self.open = false;
        Outcome::Closed(choice)
    }

    /// Moves the selection to `to` among the shown, clamped; unchanged is
    /// consumed.
    fn select(&mut self, to: usize) -> Outcome {
        let to = to.min(self.shown.len().saturating_sub(1));
        if to == self.selected {
            return Outcome::Consumed;
        }
        self.selected = to;
        self.reveal();
        Outcome::Changed
    }

    fn activate(&mut self) -> Outcome {
        let Some(index) = self.selected() else {
            return Outcome::Consumed;
        };
        let Some(entry) = self.listing.entries.get(index) else {
            return Outcome::Consumed;
        };
        if !entry.enabled {
            return Outcome::Consumed;
        }
        match (entry.kind, self.choose) {
            (Kind::Folder, _) => Outcome::Descend(index),
            (Kind::File, Choose::File) => self.close(Choice::Entry(index)),
            (Kind::File, Choose::Folder) => Outcome::Consumed,
        }
    }

    fn accept(&mut self) -> Outcome {
        match self.choose {
            Choose::Folder => self.close(Choice::Here),
            Choose::File => match self.selected_entry() {
                Some(entry) if entry.enabled && entry.kind == Kind::File => {
                    let Some(index) = self.selected() else {
                        return Outcome::Consumed;
                    };
                    self.close(Choice::Entry(index))
                }
                _ => Outcome::Consumed,
            },
        }
    }

    pub fn event(&mut self, event: Event) -> Outcome {
        if !self.open {
            return Outcome::Ignored;
        }
        match event {
            Event::Resize { surface, rect } => {
                let (filter, list) = match Self::layout(surface, rect) {
                    Ok(layout) => layout,
                    Err(error) => return self.close(Choice::Unavailable(error)),
                };
                self.surface = surface;
                self.rect = rect;
                self.filter = filter;
                self.list = list;
                self.reveal();
                Outcome::Changed
            }
            Event::Key { key, repeated } => match key {
                Key::Up => self.select(self.selected.saturating_sub(1)),
                Key::Down => self.select(self.selected.saturating_add(1)),
                Key::PageUp => self.select(self.selected.saturating_sub(self.list.rows())),
                Key::PageDown => self.select(self.selected.saturating_add(self.list.rows())),
                Key::Home => self.select(0),
                Key::End => self.select(usize::MAX),
                Key::Backspace => {
                    if self.query.pop().is_some() {
                        self.refresh(None);
                        Outcome::Changed
                    } else if repeated {
                        Outcome::Consumed
                    } else {
                        Outcome::Ascend
                    }
                }
                _ if repeated => Outcome::Consumed,
                Key::Activate => self.activate(),
                Key::Accept => self.accept(),
                Key::Parent => Outcome::Ascend,
                Key::Escape => self.close(Choice::Cancelled),
            },
            Event::Insert(character) => {
                if !filter::insert(&mut self.query, character) {
                    return Outcome::Consumed;
                }
                self.refresh(None);
                Outcome::Changed
            }
            // Pointer input off the finder is the consumer's, ignored here;
            // on it, a press picks the shown row under it and the rest is
            // consumed.
            Event::Press { x, y }
            | Event::Release { x, y }
            | Event::Move { x, y }
            | Event::Wheel { x, y, .. }
                if !self.rect.contains(x, y) =>
            {
                Outcome::Ignored
            }
            Event::Press { x, y } => match self
                .list
                .hit(x, y)
                .map(|row| self.first.saturating_add(row))
                .filter(|position| *position < self.shown.len())
            {
                Some(position) => self.select(position),
                None => Outcome::Consumed,
            },
            Event::Wheel { x, y, rows } => {
                if self.shown.is_empty() || !self.list.rect().contains(x, y) {
                    return Outcome::Consumed;
                }
                let first = self
                    .first
                    .saturating_add_signed(rows)
                    .min(self.shown.len().saturating_sub(self.list.rows()));
                let last = first
                    .saturating_add(self.list.rows())
                    .min(self.shown.len())
                    .saturating_sub(1);
                let selected = self.selected.clamp(first, last.max(first));
                if first == self.first && selected == self.selected {
                    return Outcome::Consumed;
                }
                self.first = first;
                self.selected = selected;
                Outcome::Changed
            }
            Event::Release { .. } | Event::Move { .. } | Event::Other => Outcome::Consumed,
        }
    }

    /// Paints the finder: chrome under the whole rectangle, the path's
    /// tail on the first row, the filter field, the list with the
    /// selection, and the status row under a rule. Allocates nothing.
    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        if !self.open {
            return;
        }
        let scale = self.surface.scale;
        let s = scale.value() as i64;
        fill(self.rect, CHROME, damage, sink);
        let columns = (self.rect.width as usize / (CELL_WIDTH * scale.value())).saturating_sub(2);
        // The path's tail, since the folder's own name is the useful end.
        let path = self.path_rect();
        let count = self.listing.path.chars().count();
        let at = (path.x + INSET.0 * s, path.y + INSET.1 * s);
        if count > columns {
            let skip = count - columns + 1;
            text_run(
                scale,
                std::iter::once('\u{2026}').chain(self.listing.path.chars().skip(skip)),
                at,
                path,
                GlyphStyle::medium(INK, CHROME),
                damage,
                sink,
            );
        } else {
            text_run(
                scale,
                self.listing.path.chars(),
                at,
                path,
                GlyphStyle::medium(INK, CHROME),
                damage,
                sink,
            );
        }
        let caret = self.query.chars().count();
        self.filter.emit(
            Field {
                text: &self.query,
                placeholder: "Filter",
                caret,
                anchor: None,
                first: self.filter.reveal(caret, caret, 0),
                masked: false,
                focused: true,
                caret_visible: true,
            },
            damage,
            sink,
        );
        // A meta is shown only where the row keeps `LABEL_COLUMNS` of the
        // label beside it, since the row painter reserves the meta's cells
        // first; the mark prefix takes two.
        let row_columns = (self.list.body().width as usize / (CELL_WIDTH * scale.value()))
            .saturating_sub(2 + 2 + LABEL_COLUMNS);
        self.list.emit(
            self.shown
                .iter()
                .skip(self.first)
                .filter_map(|index| self.listing.entries.get(*index))
                .map(|entry| {
                    let meta = match (entry.kind, entry.meta.is_empty()) {
                        (Kind::Folder, true) => "folder",
                        _ => &entry.meta,
                    };
                    Item {
                        label: &entry.name,
                        meta: if meta.chars().count() <= row_columns {
                            meta
                        } else {
                            ""
                        },
                        enabled: entry.enabled,
                        marked: entry.marked,
                    }
                }),
            self.first,
            self.selected,
            self.shown.len(),
            damage,
            sink,
        );
        if self.shown.is_empty() {
            let body = self.list.body();
            text_run(
                scale,
                if self.listing.entries.is_empty() {
                    "Empty folder"
                } else {
                    "No match"
                }
                .chars(),
                (body.x + INSET.0 * s, body.y + INSET.1 * s),
                body,
                GlyphStyle::medium(LINE_NUMBER, CHROME),
                damage,
                sink,
            );
        }
        let status = self.status_rect();
        fill(
            Rect {
                height: s as u32,
                ..status
            },
            BORDER,
            damage,
            sink,
        );
        let style = GlyphStyle::medium(LINE_NUMBER, CHROME);
        let at = (status.x + INSET.0 * s, status.y + INSET.1 * s);
        if !self.note.is_empty() {
            text_run(scale, self.note.chars(), at, status, style, damage, sink);
        } else {
            let total = self.listing.entries.len();
            let cut = if self.listing.truncated {
                ", cut short"
            } else {
                ""
            };
            if self.query.is_empty() {
                let word = if total == 1 { " entry" } else { " entries" };
                text_run(
                    scale,
                    Digits::new(total).chain(word.chars()).chain(cut.chars()),
                    at,
                    status,
                    style,
                    damage,
                    sink,
                );
            } else {
                text_run(
                    scale,
                    Digits::new(self.shown.len())
                        .chain(" of ".chars())
                        .chain(Digits::new(total))
                        .chain(" match".chars())
                        .chain(cut.chars()),
                    at,
                    status,
                    style,
                    damage,
                    sink,
                );
            }
        }
    }
}

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(area) = rect.intersection(damage) {
        sink(Draw {
            clip: area,
            primitive: Primitive::Fill { rect: area, color },
        });
    }
}

/// A number's decimal digits as characters, without allocating.
struct Digits {
    value: usize,
    divisor: usize,
}
impl Digits {
    fn new(value: usize) -> Self {
        let mut divisor = 1;
        while value / divisor >= 10 {
            divisor *= 10;
        }
        Self { value, divisor }
    }
}
impl Iterator for Digits {
    type Item = char;
    fn next(&mut self) -> Option<char> {
        if self.divisor == 0 {
            return None;
        }
        let digit = (self.value / self.divisor) % 10;
        self.divisor /= 10;
        char::from_digit(digit as u32, 10)
    }
}

/// Whether every ASCII-whitespace-separated term of the query occurs in
/// the name: the launcher's `filter::matches` rule over a name folded at
/// the comparison, since the query is folded ASCII (`filter::insert`) and
/// the finder keeps no folded copy of each name.
pub fn matches(name: &str, query: &str) -> bool {
    query
        .split_ascii_whitespace()
        .all(|term| contains_ignoring_ascii_case(name.as_bytes(), term.as_bytes()))
}

fn contains_ignoring_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}
