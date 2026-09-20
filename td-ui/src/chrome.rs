//! The chrome bands td-owned windows share, over `raster`: the menu bar
//! with its panel, a wrapped text block, a tab strip, a button strip, a
//! status row, a scrolling list and a single-line text entry. Each is a
//! geometry over a `Surface` in the reference renderer's units (24-pixel
//! rows of 8x16 cells, scaled by the surface) and a painter that streams
//! the complete band inside a damage rectangle. Nothing here reads a
//! clock, a file or the environment.

use crate::raster::{
    text_run, Draw, GlyphStyle, Primitive, Rect, Scale, Scrollbar, Surface, BORDER, CHROME,
    INACTIVE_SELECTION, INK, LINE_NUMBER, PAPER, SELECTED,
};
use crate::{CELL_HEIGHT, CELL_WIDTH};

/// A chrome row in font pixels: the bar's, a tab's, a panel row's and the
/// status row's.
pub const ROW: usize = 24;
/// A panel's width in font pixels.
pub const PANEL_WIDTH: usize = 320;
/// The rows a panel may have.
pub const PANEL_ROWS: usize = 13;
/// A tab's width in font pixels, and its close mark's.
pub const TAB_WIDTH: usize = 160;
pub const CLOSE_WIDTH: usize = 24;
/// The cells a status line shows at most.
pub const STATUS_COLUMNS: usize = 512;
/// The rows a text block may reserve, and the scalars its painter visits
/// at most.
pub const BLOCK_ROWS: usize = 15;
pub const BLOCK_SCALARS: usize = BLOCK_ROWS * 73;
/// A panel's selected-row background and its disabled ink.
pub const SELECTED_ROW: u32 = 0xffc9c1b2;
pub const DISABLED: u32 = 0xff827a6d;

/// The text inset inside a chrome row: one cell in, four pixels down.
const INSET: (i64, i64) = (8, 4);

/// The band above and below a button strip's buttons, in font pixels,
/// so two strips stacked keep their bezels apart.
pub const BUTTON_MARGIN: usize = 2;

/// A `List`'s scrollbar: the gutter reserved at its right and the track's
/// width within it, in font pixels, matching the editor's document bar.
const SCROLL_GUTTER: usize = 16;
const SCROLL_TRACK: usize = 12;

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(area) = rect.intersection(damage) {
        sink(Draw {
            clip: area,
            primitive: Primitive::Fill { rect: area, color },
        });
    }
}

/// A bordered action with shared text, focus and disabled styling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Button {
    surface: Surface,
    rect: Rect,
}
impl Button {
    pub fn new(surface: Surface, rect: Rect) -> Option<Self> {
        surface.check().ok()?;
        (rect.width > 0 && rect.height > 0 && rect.intersection(surface.bounds()) == Some(rect))
            .then_some(Self { surface, rect })
    }
    pub fn hit(self, x: i64, y: i64) -> bool {
        self.rect.contains(x, y)
    }
    pub fn rect(self) -> Rect {
        self.rect
    }
    pub fn emit(
        self,
        text: &str,
        selected: bool,
        enabled: bool,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let s = self.surface.scale.value() as u32;
        let outer = self.rect;
        let inset_x = s.min(outer.width / 2);
        let inset_y = s.min(outer.height / 2);
        let inner = Rect {
            x: outer.x + i64::from(inset_x),
            y: outer.y + i64::from(inset_y),
            width: outer.width.saturating_sub(2 * inset_x),
            height: outer.height.saturating_sub(2 * inset_y),
        };
        let background = if selected && enabled { SELECTED } else { PAPER };
        fill(outer, BORDER, damage, sink);
        fill(inner, background, damage, sink);
        // The text sits centred in the button's height: `INSET.1` down in
        // a `ROW`-tall one, nearer the top in a shorter one.
        let text_y = (i64::from(outer.height) - (CELL_HEIGHT * s as usize) as i64).max(0) / 2;
        text_run(
            self.surface.scale,
            text.chars(),
            (outer.x + INSET.0 * i64::from(s), outer.y + text_y),
            inner,
            GlyphStyle::medium(
                if !enabled {
                    DISABLED
                } else if selected {
                    PAPER
                } else {
                    INK
                },
                background,
            ),
            damage,
            sink,
        );
    }
}

/// A row of bordered buttons on one `ROW`-tall band at `y`: the buttons
/// from cell one, each its label's cells and a cell each side (so the
/// first label starts at cell two), one cell between, and
/// `BUTTON_MARGIN` of the band above and below. A button the surface
/// cannot hold whole is neither painted nor a target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Buttons<'a> {
    surface: Surface,
    y: i64,
    labels: &'a [&'a str],
}

impl<'a> Buttons<'a> {
    pub fn new(surface: Surface, y: i64, labels: &'a [&'a str]) -> Self {
        Self { surface, y, labels }
    }

    fn scale(self) -> usize {
        self.surface.scale.value()
    }

    /// The strip's band.
    pub fn rect(self) -> Rect {
        Rect {
            x: 0,
            y: self.y,
            width: self.surface.width as u32,
            height: (ROW * self.scale()) as u32,
        }
    }

    /// Each label's button in one pass over the labels, `None` for one
    /// the surface does not hold whole (or a geometry past the integer
    /// range, which no surface holds either).
    fn buttons(self) -> impl Iterator<Item = Option<Button>> + 'a {
        let s = self.scale();
        let top = self.y.checked_add((BUTTON_MARGIN * s) as i64);
        let height = ((ROW - 2 * BUTTON_MARGIN) * s) as u32;
        let mut column = 1usize;
        self.labels.iter().map(move |label| {
            let columns = label.chars().count().saturating_add(2);
            let x = column.checked_mul(CELL_WIDTH * s);
            column = column.saturating_add(columns).saturating_add(1);
            let rect = Rect {
                x: i64::try_from(x?).ok()?,
                y: top?,
                width: u32::try_from(columns.checked_mul(CELL_WIDTH * s)?).ok()?,
                height,
            };
            Button::new(self.surface, rect)
        })
    }

    /// Button `index`, when the surface holds it whole.
    pub fn button(self, index: usize) -> Option<Button> {
        self.buttons().nth(index)?
    }

    /// The button holding the point.
    pub fn hit(self, x: i64, y: i64) -> Option<usize> {
        self.buttons()
            .position(|button| button.is_some_and(|b| b.hit(x, y)))
    }

    /// Paints the band chrome, then each button the surface holds whole
    /// with its `(selected, enabled)` from `states`, in label order; a
    /// label `states` runs out before is painted neither selected nor
    /// disabled.
    pub fn emit(
        &self,
        states: impl IntoIterator<Item = (bool, bool)>,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let Some(damage) = damage.intersection(self.surface.bounds()) else {
            return;
        };
        fill(self.rect(), CHROME, damage, sink);
        let mut states = states.into_iter();
        for (label, button) in self.labels.iter().zip(self.buttons()) {
            let (selected, enabled) = states.next().unwrap_or((false, true));
            if let Some(button) = button {
                button.emit(label, selected, enabled, damage, sink);
            }
        }
    }
}

/// One list row as painted, the shared model behind `Panel` and `List`:
/// its background chosen by `selected`, its ink by `enabled`, `prefix`
/// then `label` from the row's first cell and `trailing` at its right.
struct RowPaint<'a> {
    rect: Rect,
    selected: bool,
    enabled: bool,
    prefix: &'a str,
    label: &'a str,
    trailing: &'a str,
}

fn paint_row(scale: Scale, row: RowPaint, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    let s = scale.value() as i64;
    let background = if row.selected { SELECTED_ROW } else { CHROME };
    let ink = if row.enabled { INK } else { DISABLED };
    let style = GlyphStyle::medium(ink, background);
    fill(row.rect, background, damage, sink);
    let cells = row.trailing.chars().count() as i64;
    let start = row.rect.x + i64::from(row.rect.width) - (INSET.0 + cells * CELL_WIDTH as i64) * s;
    // The label stops at the trailing column so a long label cannot
    // overpaint it; the menu's labels never reach it, so its pixels hold.
    let label_bounds = Rect {
        width: (start - row.rect.x).max(0) as u32,
        ..row.rect
    };
    text_run(
        scale,
        row.prefix.chars().chain(row.label.chars()),
        (row.rect.x + INSET.0 * s, row.rect.y + INSET.1 * s),
        label_bounds,
        style,
        damage,
        sink,
    );
    text_run(
        scale,
        row.trailing.chars(),
        (start, row.rect.y + INSET.1 * s),
        row.rect,
        style,
        damage,
        sink,
    );
}

/// The menu bar: `labels` on the first row from cell column one, three
/// cells between them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bar<'a> {
    labels: &'a [&'a str],
    surface: Surface,
}

impl<'a> Bar<'a> {
    pub fn new(surface: Surface, labels: &'a [&'a str]) -> Self {
        Self { labels, surface }
    }

    fn scale(self) -> usize {
        self.surface.scale.value()
    }

    /// The bar's row.
    pub fn rect(self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.surface.width as u32,
            height: (ROW * self.scale()) as u32,
        }
    }

    /// The header of label `index`: its cells and the three after it,
    /// none after the last.
    pub fn header(self, index: usize) -> Option<Rect> {
        let s = self.scale();
        let last = self.labels.len().checked_sub(1)?;
        let mut column = 1;
        for (i, label) in self.labels.iter().enumerate() {
            let columns = label.chars().count() + if i == last { 0 } else { 3 };
            if i == index {
                return Some(Rect {
                    x: (column * CELL_WIDTH * s) as i64,
                    y: 0,
                    width: (columns * CELL_WIDTH * s) as u32,
                    height: (ROW * s) as u32,
                });
            }
            column += columns;
        }
        None
    }

    /// The label whose header holds the point.
    pub fn hit(self, x: i64, y: i64) -> Option<usize> {
        (0..self.labels.len()).find(|&index| self.header(index).is_some_and(|r| r.contains(x, y)))
    }

    /// Paints the row: chrome, then the labels joined by three spaces.
    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(damage) = damage.intersection(self.surface.bounds()) else {
            return;
        };
        let s = self.scale() as i64;
        let rect = self.rect();
        fill(rect, CHROME, damage, sink);
        let last = self.labels.len().saturating_sub(1);
        let chars = self.labels.iter().enumerate().flat_map(|(i, label)| {
            label
                .chars()
                .chain(if i == last { "" } else { "   " }.chars())
        });
        text_run(
            self.surface.scale,
            chars,
            (INSET.0 * s, INSET.1 * s),
            rect,
            GlyphStyle::medium(INK, CHROME),
            damage,
            sink,
        );
    }
}

/// One panel row as painted: its label, the shortcut shown at its right,
/// whether it can be chosen and whether it carries the check mark.
#[derive(Clone, Copy, Debug)]
pub struct Row<'a> {
    pub label: &'a str,
    pub shortcut: &'a str,
    pub enabled: bool,
    pub checked: bool,
}

/// A menu panel under its header: `PANEL_WIDTH` wide, one `ROW` per
/// entry, complete inside the surface with a status row below it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Panel {
    rect: Rect,
    scale: Scale,
}

impl Panel {
    /// The panel of `rows` under `header`, its left edge clamped to the
    /// surface's right edge; `None` when there are no rows or too many,
    /// or the surface cannot hold the panel whole above a status row.
    pub fn new(surface: Surface, header: Rect, rows: usize) -> Option<Self> {
        if rows == 0 || rows > PANEL_ROWS {
            return None;
        }
        let s = surface.scale.value();
        let width = PANEL_WIDTH * s;
        let height = rows * ROW * s;
        let y = header.y.checked_add(i64::from(header.height))?;
        let bottom = y
            .checked_add(height as i64)?
            .checked_add((ROW * s) as i64)?;
        if y < 0 || surface.width < width || bottom > surface.height as i64 {
            return None;
        }
        Some(Self {
            rect: Rect {
                x: header.x.min((surface.width - width) as i64).max(0),
                y,
                width: width as u32,
                height: height as u32,
            },
            scale: surface.scale,
        })
    }

    /// A panel in caller-selected, fully visible geometry. Menus use this
    /// after fitting or scrolling; the legacy header constructor stays strict.
    pub fn within(surface: Surface, rect: Rect) -> Option<Self> {
        let row = ROW * surface.scale.value();
        if rect.intersection(surface.bounds()) != Some(rect)
            || rect.width == 0
            || rect.height == 0
            || !(rect.height as usize).is_multiple_of(row)
            || rect.height as usize / row > PANEL_ROWS
        {
            return None;
        }
        Some(Self {
            rect,
            scale: surface.scale,
        })
    }

    pub fn rect(self) -> Rect {
        self.rect
    }

    pub fn rows(self) -> usize {
        self.rect.height as usize / (ROW * self.scale.value())
    }

    /// The row holding the point.
    pub fn hit(self, x: i64, y: i64) -> Option<usize> {
        self.rect
            .contains(x, y)
            .then(|| (y - self.rect.y) as usize / (ROW * self.scale.value()))
    }

    /// Row `index`'s rectangle.
    pub fn row(self, index: usize) -> Option<Rect> {
        if index >= self.rows() {
            return None;
        }
        let height = (ROW * self.scale.value()) as i64;
        Some(Rect {
            y: self.rect.y + index as i64 * height,
            height: height as u32,
            ..self.rect
        })
    }

    /// Paints the rows in order, the selected one highlighted, a disabled
    /// one dim, a checked one prefixed with a plus, each shortcut at the
    /// row's right edge; rows beyond the panel's are not painted.
    pub fn emit<'a>(
        &self,
        rows: impl IntoIterator<Item = Row<'a>>,
        selected: usize,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        for (index, row) in rows.into_iter().enumerate() {
            let Some(rect) = self.row(index) else {
                break;
            };
            paint_row(
                self.scale,
                RowPaint {
                    rect,
                    selected: index == selected,
                    enabled: row.enabled,
                    prefix: if row.checked { "+ " } else { "  " },
                    label: row.label,
                    trailing: row.shortcut,
                },
                damage,
                sink,
            );
        }
    }
}

/// One list row: its label, an optional right-aligned column, whether it
/// can be chosen and whether it carries the multi-select mark.
#[derive(Clone, Copy, Debug)]
pub struct Item<'a> {
    pub label: &'a str,
    pub meta: &'a str,
    pub enabled: bool,
    pub marked: bool,
}

/// A scrolling list filling a rectangle: `ROW`-tall rows painted by the
/// panel's row painter, a marked row prefixed, an optional right-aligned
/// column, and a scrollbar in the gutter at its right. The caller owns the
/// selection and the first shown item; the list holds no state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct List {
    rect: Rect,
    scale: Scale,
}

impl List {
    /// The list filling `rect`; `None` when `rect` lies outside the surface
    /// or cannot hold one `ROW` beside the scrollbar gutter.
    pub fn new(surface: Surface, rect: Rect) -> Option<Self> {
        let s = surface.scale.value();
        if rect.intersection(surface.bounds()) != Some(rect) {
            return None;
        }
        if rect.width as usize <= SCROLL_GUTTER * s || (rect.height as usize) < ROW * s {
            return None;
        }
        Some(Self {
            rect,
            scale: surface.scale,
        })
    }

    pub fn rect(self) -> Rect {
        self.rect
    }

    /// The rows the list shows at once.
    pub fn rows(self) -> usize {
        self.rect.height as usize / (ROW * self.scale.value())
    }

    /// The row area: the rect less the scrollbar gutter and any remainder
    /// below the last whole row, for a caller clipping its own content to
    /// the rows.
    pub fn body(self) -> Rect {
        let s = self.scale.value();
        let gutter = (SCROLL_GUTTER * s) as u32;
        Rect {
            width: self.rect.width.saturating_sub(gutter),
            height: (self.rows() * ROW * s) as u32,
            ..self.rect
        }
    }

    /// Visible row `index`'s rectangle.
    pub fn row(self, index: usize) -> Option<Rect> {
        if index >= self.rows() {
            return None;
        }
        let height = (ROW * self.scale.value()) as i64;
        let body = self.body();
        Some(Rect {
            y: body.y + index as i64 * height,
            height: height as u32,
            ..body
        })
    }

    /// The visible row holding the point, among the shown rows.
    pub fn hit(self, x: i64, y: i64) -> Option<usize> {
        let body = self.body();
        if !body.contains(x, y) {
            return None;
        }
        let row = (y - body.y) as usize / (ROW * self.scale.value());
        (row < self.rows()).then_some(row)
    }

    /// The scrollbar for `total` items scrolled to `first`.
    pub fn scrollbar(self, total: usize, first: usize) -> Scrollbar {
        let s = self.scale.value();
        let track = Rect {
            x: self.rect.x + i64::from(self.rect.width) - (SCROLL_GUTTER * s) as i64,
            y: self.rect.y,
            width: (SCROLL_TRACK * s) as u32,
            height: (self.rows() * ROW * s) as u32,
        };
        Scrollbar::new(track, self.rows(), total, first, self.scale, false)
    }

    /// `first` moved as little as possible so `selected` is a shown row.
    pub fn reveal(self, total: usize, selected: usize, first: usize) -> usize {
        let rows = self.rows();
        if rows == 0 || total == 0 {
            return 0;
        }
        let selected = selected.min(total - 1);
        let first = first.min(total.saturating_sub(rows));
        if selected < first {
            selected
        } else if selected >= first + rows {
            selected + 1 - rows
        } else {
            first
        }
    }

    /// Paints the window `first..` given by `items`, the item at `selected`
    /// highlighted, a marked one prefixed, empty rows below left chrome, and
    /// the scrollbar for `total` items; items past the shown rows are not
    /// painted.
    pub fn emit<'a>(
        &self,
        items: impl IntoIterator<Item = Item<'a>>,
        first: usize,
        selected: usize,
        total: usize,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        // The whole rect is chrome first, like the other bands, so the
        // gutter, its margin and any remainder below the last row are
        // painted; the rows and thumb draw over it.
        fill(self.rect, CHROME, damage, sink);
        for (i, item) in items.into_iter().enumerate() {
            let Some(rect) = self.row(i) else {
                break;
            };
            paint_row(
                self.scale,
                RowPaint {
                    rect,
                    selected: selected.checked_sub(first) == Some(i),
                    enabled: item.enabled,
                    prefix: if item.marked { "* " } else { "  " },
                    label: item.label,
                    trailing: item.meta,
                },
                damage,
                sink,
            );
        }
        let bar = self.scrollbar(total, first);
        fill(
            bar.thumb,
            if bar.enabled() { LINE_NUMBER } else { BORDER },
            damage,
            sink,
        );
    }
}

/// The glyph a masked field shows in place of each of its characters.
const MASK: char = '\u{2022}';

/// What a `TextEntry` paints: the plaintext, the caret and an optional
/// selection anchor in character columns, the first shown column, and the
/// display options. Masking is a display choice the caller makes; the
/// widget draws `MASK` for each character and asserts nothing about the
/// field's trust, which is the consumer's to establish (see DESIGN.md).
#[derive(Clone, Copy, Debug)]
pub struct Field<'a> {
    /// The field's whole value; the caller owns and clamps it.
    pub text: &'a str,
    /// Shown dim when `text` is empty; `""` for none. Never masked.
    pub placeholder: &'a str,
    /// The caret's character column, `0..=text.chars().count()`.
    pub caret: usize,
    /// The selection's other end; the range is `anchor..caret`. `None`, or
    /// equal to `caret`, is no selection.
    pub anchor: Option<usize>,
    /// The first shown character column; `reveal` keeps the caret in view.
    pub first: usize,
    /// Draw `MASK` for each character instead of the character itself.
    pub masked: bool,
    /// The active-window selection colour and caret; the caller's focus.
    pub focused: bool,
    /// Whether the caret shows; the caller owns the blink and hides it
    /// when unfocused.
    pub caret_visible: bool,
}

/// A single-line text field over one chrome row: a paper ground, the text
/// from the first shown column, an optional selection and a one-pixel
/// caret. The caller owns the text, the caret, the selection and the first
/// shown column; the field holds no state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextEntry {
    rect: Rect,
    scale: Scale,
}

impl TextEntry {
    /// The field filling `rect`; `None` when `rect` lies outside the
    /// surface or cannot hold a text cell between the insets on one `ROW`.
    pub fn new(surface: Surface, rect: Rect) -> Option<Self> {
        let s = surface.scale.value();
        if rect.intersection(surface.bounds()) != Some(rect) {
            return None;
        }
        if (rect.width as usize) < (2 * INSET.0 as usize + CELL_WIDTH) * s
            || (rect.height as usize) < ROW * s
        {
            return None;
        }
        Some(Self {
            rect,
            scale: surface.scale,
        })
    }

    pub fn rect(self) -> Rect {
        self.rect
    }

    /// The text cells the field shows at once, the width less a cell inset
    /// each side.
    pub fn columns(self) -> usize {
        let s = self.scale.value();
        (self.rect.width as usize).saturating_sub(2 * INSET.0 as usize * s) / (CELL_WIDTH * s)
    }

    fn cell(self) -> i64 {
        (CELL_WIDTH * self.scale.value()) as i64
    }

    fn text_x(self) -> i64 {
        self.rect.x + INSET.0 * self.scale.value() as i64
    }

    fn text_y(self) -> i64 {
        self.rect.y + INSET.1 * self.scale.value() as i64
    }

    fn glyph_h(self) -> u32 {
        (CELL_HEIGHT * self.scale.value()) as u32
    }

    /// `first` moved as little as possible so column `caret` shows, given
    /// the text length.
    pub fn reveal(self, len: usize, caret: usize, first: usize) -> usize {
        let cols = self.columns();
        if cols == 0 {
            return 0;
        }
        let caret = caret.min(len);
        // The caret is a boundary between cells with `cols + 1` shown
        // positions, `first..=first + cols`; the last sits in the right
        // inset, so `caret == first + cols` is shown and does not scroll.
        let first = first.min(len.saturating_sub(cols));
        if caret < first {
            caret
        } else if caret > first + cols {
            caret - cols
        } else {
            first
        }
    }

    /// The caret column a point falls on, clamped to the text, given the
    /// first shown column and the text length; `None` outside the field.
    pub fn hit(self, x: i64, y: i64, first: usize, len: usize) -> Option<usize> {
        if !self.rect.contains(x, y) {
            return None;
        }
        // Clamp to the last shown column so the right inset maps to the
        // rightmost visible caret, not an off-window one, then to the text.
        let column = ((x - self.text_x()).max(0) / self.cell()) as usize;
        Some(first.saturating_add(column.min(self.columns())).min(len))
    }

    /// Paints the field: the paper ground, the selection, the visible text
    /// or the placeholder, and the caret.
    pub fn emit(&self, field: Field, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        fill(self.rect, PAPER, damage, sink);
        let len = field.text.chars().count();
        // A stale caret, anchor or first is clamped to the text so the
        // field renders and cannot overflow; the caller drives `first`
        // with `reveal`.
        let field = Field {
            caret: field.caret.min(len),
            anchor: field.anchor.map(|a| a.min(len)),
            first: field.first.min(len),
            ..field
        };
        if len == 0 {
            if !field.placeholder.is_empty() {
                text_run(
                    self.scale,
                    field.placeholder.chars().take(self.columns()),
                    (self.text_x(), self.text_y()),
                    self.rect,
                    GlyphStyle::medium(LINE_NUMBER, PAPER),
                    damage,
                    sink,
                );
            }
            self.caret(field, damage, sink);
            return;
        }
        let cols = self.columns();
        let end = field.first.saturating_add(cols).min(len);
        let start = field.first.min(end);
        // The selection's visible columns; no selection is no split.
        let (lo, hi) = match field.anchor {
            Some(anchor) if anchor != field.caret => (
                anchor.min(field.caret).clamp(start, end),
                anchor.max(field.caret).clamp(start, end),
            ),
            _ => (start, start),
        };
        if hi > lo {
            fill(
                Rect {
                    x: self.text_x() + (lo - field.first) as i64 * self.cell(),
                    y: self.text_y(),
                    width: ((hi - lo) as i64 * self.cell()) as u32,
                    height: self.glyph_h(),
                },
                self.selection(field.focused).0,
                damage,
                sink,
            );
        }
        self.run(field, start, lo, false, damage, sink);
        self.run(field, lo, hi, true, damage, sink);
        self.run(field, hi, end, false, damage, sink);
        self.caret(field, damage, sink);
    }

    /// A selection's background and its ink, by focus.
    fn selection(self, focused: bool) -> (u32, u32) {
        if focused {
            (SELECTED, PAPER)
        } else {
            (INACTIVE_SELECTION, INK)
        }
    }

    /// One glyph run of columns `from..to`, selected or not.
    fn run(
        self,
        field: Field,
        from: usize,
        to: usize,
        selected: bool,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        if to <= from {
            return;
        }
        let (background, ink) = if selected {
            self.selection(field.focused)
        } else {
            (PAPER, INK)
        };
        let style = GlyphStyle::medium(ink, background);
        let at = (
            self.text_x() + (from - field.first) as i64 * self.cell(),
            self.text_y(),
        );
        let chars = field.text.chars().skip(from).take(to - from);
        if field.masked {
            text_run(
                self.scale,
                chars.map(|_| MASK),
                at,
                self.rect,
                style,
                damage,
                sink,
            );
        } else {
            text_run(self.scale, chars, at, self.rect, style, damage, sink);
        }
    }

    /// The one-pixel caret, when shown and inside the window.
    fn caret(self, field: Field, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        if !field.caret_visible
            || field.caret < field.first
            || field.caret > field.first + self.columns()
        {
            return;
        }
        fill(
            Rect {
                x: self.text_x() + (field.caret - field.first) as i64 * self.cell(),
                y: self.text_y(),
                width: self.scale.value() as u32,
                height: self.glyph_h(),
            },
            INK,
            damage,
            sink,
        );
    }
}

/// The next row from `selected` among `count`, forward or backward with
/// wrap, skipping rows `enabled` refuses; `selected` again after a full
/// turn finds none.
pub fn step(
    count: usize,
    selected: usize,
    backward: bool,
    enabled: impl Fn(usize) -> bool,
) -> usize {
    let mut next = selected;
    for _ in 0..count {
        next = if backward {
            (next + count - 1) % count
        } else {
            (next + 1) % count
        };
        if enabled(next) {
            break;
        }
    }
    next
}

/// Keyboard selection among tabs, independent of document key bindings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabNavigation {
    Previous,
    Next,
    First,
    Last,
}

/// A visible tab selection or its optional close control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabHit {
    Select(usize),
    Close(usize),
}

/// A tab strip on one row: `count` tabs `TAB_WIDTH` wide from the left,
/// the active one always in the strip; a surface narrower than one tab
/// clips that tab.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Strip {
    surface: Surface,
    y: i64,
    active: usize,
    count: usize,
    closable: bool,
}

impl Strip {
    /// The strip always fills its row; `None` only when there are tabs but
    /// `active` is not one of them.
    pub fn new(surface: Surface, y: i64, active: usize, count: usize) -> Option<Self> {
        (count == 0 || active < count).then_some(Self {
            surface,
            y,
            active,
            count,
            closable: true,
        })
    }

    /// Document tabs show close buttons by default. Resource tabs opt out;
    /// their labels use that space and cannot produce a Close hit.
    pub fn with_close_buttons(mut self, closable: bool) -> Self {
        self.closable = closable;
        self
    }

    pub fn selection(self, navigation: TabNavigation) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        Some(match navigation {
            TabNavigation::First => 0,
            TabNavigation::Last => self.count - 1,
            TabNavigation::Previous => {
                if self.active == 0 {
                    self.count - 1
                } else {
                    self.active - 1
                }
            }
            TabNavigation::Next => self
                .active
                .checked_add(1)
                .filter(|index| *index < self.count)
                .unwrap_or(0),
        })
    }

    /// A hit in the visible part of this row. A clipped tab has no hit
    /// outside the surface, including its off-screen close button.
    pub fn hit(self, x: i64, y: i64) -> Option<TabHit> {
        self.surface.check().ok()?;
        if !self.surface.bounds().contains(x, y) || !self.rect().contains(x, y) {
            return None;
        }
        let (width, _, first) = self.visible_layout();
        let index = first.checked_add(x as usize / width)?;
        self.tab(index)?;
        Some(
            if self.close(index).is_some_and(|rect| rect.contains(x, y)) {
                TabHit::Close(index)
            } else {
                TabHit::Select(index)
            },
        )
    }

    fn visible_layout(self) -> (usize, usize, usize) {
        let width = TAB_WIDTH * self.scale();
        let visible = (self.surface.width / width).max(1);
        (width, visible, self.active.saturating_sub(visible - 1))
    }

    fn scale(self) -> usize {
        self.surface.scale.value()
    }

    /// The strip's row.
    pub fn rect(self) -> Rect {
        Rect {
            x: 0,
            y: self.y,
            width: self.surface.width as u32,
            height: (ROW * self.scale()) as u32,
        }
    }

    /// Tab `index`'s rectangle, `None` when it is not a tab or is scrolled
    /// out of the strip to keep the active one in.
    pub fn tab(self, index: usize) -> Option<Rect> {
        if index >= self.count {
            return None;
        }
        let s = self.scale();
        let (width, visible, first) = self.visible_layout();
        let slot = index.checked_sub(first)?;
        if slot >= visible {
            return None;
        }
        Some(Rect {
            x: (slot * width) as i64,
            y: self.y,
            width: width as u32,
            height: (ROW * s) as u32,
        })
    }

    /// Tab `index`'s close mark, its rightmost `CLOSE_WIDTH`.
    pub fn close(self, index: usize) -> Option<Rect> {
        if !self.closable {
            return None;
        }
        let tab = self.tab(index)?;
        let width = (CLOSE_WIDTH * self.scale()) as u32;
        Some(Rect {
            x: tab.x + i64::from(tab.width - width),
            width,
            ..tab
        })
    }

    /// Paints the row: chrome, then each visible tab in order from
    /// `tabs` (its title and whether it is dirty): paper for the active
    /// tab and chrome otherwise, a top and a right border, the title
    /// prefixed with a star when dirty and clipped short of the close
    /// mark, and the mark.
    pub fn emit<'a>(
        &self,
        tabs: impl IntoIterator<Item = (&'a str, bool)>,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let Some(damage) = damage.intersection(self.surface.bounds()) else {
            return;
        };
        let s = self.scale() as i64;
        fill(self.rect(), CHROME, damage, sink);
        for (index, (title, dirty)) in tabs.into_iter().enumerate() {
            let Some(rect) = self.tab(index) else {
                continue;
            };
            let close = self.close(index);
            let background = if index == self.active { PAPER } else { CHROME };
            let style = GlyphStyle::medium(INK, background);
            fill(rect, background, damage, sink);
            fill(
                Rect {
                    height: s as u32,
                    ..rect
                },
                BORDER,
                damage,
                sink,
            );
            fill(
                Rect {
                    x: rect.x + i64::from(rect.width) - s,
                    width: s as u32,
                    ..rect
                },
                BORDER,
                damage,
                sink,
            );
            let title_rect = Rect {
                width: rect
                    .width
                    .saturating_sub(close.map_or(s as u32, |close| close.width)),
                ..rect
            };
            text_run(
                self.surface.scale,
                if dirty { "*" } else { "" }.chars().chain(title.chars()),
                (rect.x + INSET.0 * s, rect.y + INSET.1 * s),
                title_rect,
                style,
                damage,
                sink,
            );
            if let Some(close) = close {
                text_run(
                    self.surface.scale,
                    "x".chars(),
                    (close.x + INSET.0 * s, close.y + INSET.1 * s),
                    close,
                    style,
                    damage,
                    sink,
                );
            }
        }
    }
}

/// A wrapped text block: `rows` cell rows across the surface at `y`, its
/// text wrapped at the block's columns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Block {
    surface: Surface,
    y: i64,
    rows: usize,
}

impl Block {
    /// `None` beyond `BLOCK_ROWS`.
    pub fn new(surface: Surface, y: i64, rows: usize) -> Option<Self> {
        (rows <= BLOCK_ROWS).then_some(Self { surface, y, rows })
    }

    fn scale(self) -> usize {
        self.surface.scale.value()
    }

    pub fn rows(self) -> usize {
        self.rows
    }

    /// The block's area: full width, its rows tall.
    pub fn rect(self) -> Rect {
        Rect {
            x: 0,
            y: self.y,
            width: self.surface.width as u32,
            height: (self.rows * CELL_HEIGHT * self.scale()) as u32,
        }
    }

    /// The cells a row holds: the width less a cell each side, at least
    /// one.
    pub fn columns(self) -> usize {
        let s = self.scale();
        (self.surface.width.saturating_sub(2 * CELL_WIDTH * s) / (CELL_WIDTH * s)).max(1)
    }

    /// Paints the block: chrome, then `text` from its top-left cell,
    /// wrapping at the columns, a newline starting the next row, at most
    /// `BLOCK_SCALARS` scalars visited and the rows that fit painted.
    pub fn emit(&self, text: &str, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(damage) = damage.intersection(self.surface.bounds()) else {
            return;
        };
        let s = self.scale();
        let rect = self.rect();
        fill(rect, CHROME, damage, sink);
        let Some(clip) = rect.intersection(damage) else {
            return;
        };
        let columns = self.columns();
        let style = GlyphStyle::medium(INK, CHROME);
        let (mut column, mut row) = (0, 0);
        for scalar in text.chars().take(BLOCK_SCALARS) {
            if scalar == '\n' || column == columns {
                row += 1;
                column = 0;
            }
            if row >= self.rows {
                break;
            }
            if scalar == '\n' {
                continue;
            }
            sink(Draw {
                clip,
                primitive: Primitive::Glyph {
                    x: ((INSET.0 as usize + column * CELL_WIDTH) * s) as i64,
                    y: rect.y + (row * CELL_HEIGHT * s) as i64,
                    scalar,
                    style,
                },
            });
            column += 1;
        }
    }
}

/// The status row: the surface's bottom `ROW`, one line of text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Status {
    surface: Surface,
}

impl Status {
    pub fn new(surface: Surface) -> Self {
        Self { surface }
    }

    fn scale(self) -> usize {
        self.surface.scale.value()
    }

    /// The bottom row, `ROW` scaled tall; a shorter surface clips it on
    /// emit, not here.
    pub fn rect(self) -> Rect {
        let height = ROW * self.scale();
        Rect {
            x: 0,
            y: self.surface.height.saturating_sub(height) as i64,
            width: self.surface.width as u32,
            height: height as u32,
        }
    }

    /// The cells a line shows: the width less a cell each side, at most
    /// `STATUS_COLUMNS`.
    pub fn columns(self) -> usize {
        let s = self.scale();
        (self.surface.width.saturating_sub(2 * CELL_WIDTH * s) / (CELL_WIDTH * s))
            .min(STATUS_COLUMNS)
    }

    /// Fills the row chrome under a one-pixel top border, without a line;
    /// a consumer that lays out its own status text frames it with this.
    pub fn frame(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(damage) = damage.intersection(self.surface.bounds()) else {
            return;
        };
        let s = self.scale() as i64;
        let rect = self.rect();
        fill(rect, CHROME, damage, sink);
        fill(
            Rect {
                height: s as u32,
                ..rect
            },
            BORDER,
            damage,
            sink,
        );
    }

    /// Paints the row: its `frame`, then `line` in whole cells from the
    /// first, a control scalar blank, and the last cell an ellipsis when
    /// the line is longer than the row shows.
    pub fn emit(&self, line: impl Iterator<Item = char>, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        self.frame(damage, sink);
        let Some(damage) = damage.intersection(self.surface.bounds()) else {
            return;
        };
        let s = self.scale() as i64;
        let rect = self.rect();
        let columns = self.columns();
        let mut chars = line.peekable();
        let shown = (0..columns).map_while(|column| {
            let scalar = chars.next()?;
            Some(if column + 1 == columns && chars.peek().is_some() {
                '…'
            } else if scalar.is_control() {
                ' '
            } else {
                scalar
            })
        });
        text_run(
            self.surface.scale,
            shown,
            (INSET.0 * s, rect.y + INSET.1 * s),
            rect,
            GlyphStyle::medium(INK, CHROME),
            damage,
            sink,
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn surface(width: usize, height: usize, scale: u8) -> Surface {
        Surface::new(width, height, Scale::new(scale).unwrap()).unwrap()
    }

    #[test]
    fn a_panel_needs_two_rows_of_headroom_and_clamps_to_the_right_edge() {
        for scale in 1..=4u8 {
            let s = scale as usize;
            let header = Bar::new(surface(320 * s, 600, scale), &["A", "B"])
                .header(1)
                .unwrap();
            for rows in [1, 7, 13] {
                let height = (rows + 2) * ROW * s;
                assert!(Panel::new(surface(320 * s, height - 1, scale), header, rows).is_none());
                assert!(Panel::new(surface(320 * s - 1, height, scale), header, rows).is_none());
                let panel = Panel::new(surface(320 * s, height, scale), header, rows).unwrap();
                assert_eq!(panel.rows(), rows);
                assert_eq!(panel.rect().x, 0, "clamped");
                assert_eq!(panel.rect().y, (ROW * s) as i64);
            }
            let ample = surface(320 * s, 600, scale);
            assert!(Panel::new(ample, header, 0).is_none());
            assert!(Panel::new(ample, header, PANEL_ROWS + 1).is_none());
        }
    }

    #[test]
    fn stepping_skips_disabled_rows_and_wraps() {
        let enabled = |i: usize| i != 1 && i != 3;
        assert_eq!(step(4, 0, false, enabled), 2);
        assert_eq!(step(4, 2, false, enabled), 0);
        assert_eq!(step(4, 0, true, enabled), 2);
        assert_eq!(step(4, 0, false, |_| false), 0, "a full turn finds none");
        assert_eq!(step(0, 5, false, |_| true), 5, "no rows");
    }
}
