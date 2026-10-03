//! Where each region of the window lies on the surface: the action strip
//! across the top, the status row at the bottom, and between them the
//! locked view, the notebook's two panes or its keys, with the key prompt
//! and the confirmation dialog laid over the middle.

use td_ui::chrome::{Buttons, List, Status, TextEntry, ROW};
use td_ui::raster::{Rect, Surface};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

// Every strip ends with Quit, which closing the window also asks for.
pub const NOTEBOOK: [&str; 8] = [
    "New", "Rename", "Delete", "Save", "Find", "Keys", "Lock", "Quit",
];
pub const KEYS: [&str; 7] = [
    "Notebook",
    "Use for saves",
    "Add backup",
    "Replace",
    "Export",
    "Lock",
    "Quit",
];
pub const LOCKED: [&str; 4] = ["Unlock", "Create", "Import", "Quit"];
pub const IMPORT: [&str; 3] = ["Import", "Cancel", "Quit"];
pub const PRESENT: [&str; 2] = ["Continue", "Cancel"];
pub const PIN: [&str; 2] = ["OK", "Cancel"];

/// The notebook's panes; a region the surface is too small for is `None`
/// and is neither painted nor hit.
#[derive(Clone, Copy, Debug)]
pub struct Panes {
    pub search: Option<TextEntry>,
    pub list: Option<List>,
    pub title: Option<TextEntry>,
    /// The editor pane's outline, a bezel a scaled pixel wide round
    /// `pane`, as the fields and the list carry theirs.
    pub frame: Rect,
    pub pane: Rect,
    pub find: Option<TextEntry>,
}

pub fn row(surface: Surface) -> i64 {
    (ROW * surface.scale.value()) as i64
}

pub fn cell(surface: Surface) -> i64 {
    (CELL_WIDTH * surface.scale.value()) as i64
}

pub fn glyph_height(surface: Surface) -> i64 {
    (CELL_HEIGHT * surface.scale.value()) as i64
}

pub fn strip(surface: Surface, labels: &'static [&'static str]) -> Buttons<'static> {
    Buttons::new(surface, 0, labels)
}

/// The band between the strip and the status row.
pub fn body(surface: Surface, labels: &'static [&'static str]) -> Rect {
    let top = i64::from(strip(surface, labels).rect().height);
    let bottom = Status::new(surface).rect().y.max(top);
    Rect {
        x: 0,
        y: top,
        width: surface.width as u32,
        height: (bottom - top) as u32,
    }
}

fn rect(x: i64, y: i64, width: i64, height: i64) -> Rect {
    Rect {
        x,
        y,
        width: width.max(0) as u32,
        height: height.max(0) as u32,
    }
}

/// The search field over the title list on the left, a third of the
/// width; the title over the entry's text on the right, with the find
/// field under the text while finding. Each is outlined, so the two
/// sides meet at their outlines with no divider between.
pub fn panes(surface: Surface, finding: bool) -> Panes {
    let body = body(surface, &NOTEBOOK);
    let s = surface.scale.value() as i64;
    let row = row(surface);
    let width = i64::from(body.width);
    let height = i64::from(body.height);
    let left = (width / 3).max((20 * CELL_WIDTH) as i64 * s).min(width);
    let right_width = width - left;
    let find_height = if finding { row } else { 0 };
    let frame = rect(left, body.y + row, right_width, height - row - find_height);
    Panes {
        search: TextEntry::new(surface, rect(0, body.y, left, row)),
        list: List::new(surface, rect(0, body.y + row, left, height - row)),
        title: TextEntry::new(surface, rect(left, body.y, right_width, row)),
        frame,
        pane: rect(
            frame.x + s,
            frame.y + s,
            i64::from(frame.width) - 2 * s,
            i64::from(frame.height) - 2 * s,
        ),
        find: if finding {
            TextEntry::new(surface, rect(left, body.y + height - row, right_width, row))
        } else {
            None
        },
    }
}

/// The locked view: two message rows, then the enrolled keys.
pub fn keys(surface: Surface) -> Option<List> {
    listing(surface, &LOCKED)
}

/// The unlocked notebook's keys view, laid out as the locked view.
pub fn enrolled(surface: Surface) -> Option<List> {
    listing(surface, &KEYS)
}

/// The keys an encrypted copy opens with, laid out as the locked view.
pub fn copy_keys(surface: Surface) -> Option<List> {
    listing(surface, &IMPORT)
}

/// The finder: the body under the strip it was opened from, the locked
/// view's for an import and the keys view's for an export, a cell in
/// from each side.
pub fn finder(surface: Surface, import: bool) -> Rect {
    let body = body(surface, if import { &LOCKED } else { &KEYS });
    let cell = cell(surface);
    rect(
        cell,
        body.y,
        (surface.width as i64) - 2 * cell,
        i64::from(body.height),
    )
}

fn listing(surface: Surface, labels: &'static [&'static str]) -> Option<List> {
    let body = body(surface, labels);
    let row = row(surface);
    List::new(
        surface,
        rect(
            0,
            body.y + 2 * row,
            i64::from(body.width),
            i64::from(body.height) - 2 * row,
        ),
    )
}

/// The key prompt: the title's rows, the instruction's rows, the PIN
/// field's row and the buttons, centred and at most sixty cells wide.
#[derive(Clone, Copy, Debug)]
pub struct Prompt {
    pub rect: Rect,
    pub title: Rect,
    pub line: Rect,
    pub pin: Option<TextEntry>,
    pub buttons: Buttons<'static>,
}

/// The prompt's width, border included.
fn prompt_width(surface: Surface) -> i64 {
    let cell = cell(surface);
    ((surface.width as i64) - 2 * cell).clamp(0, 60 * cell)
}

/// The characters one of the prompt's text rows shows: its width inside
/// the border, less a cell of margin each side, as `paint::line` draws.
pub fn prompt_columns(surface: Surface) -> usize {
    let s = surface.scale.value() as i64;
    ((prompt_width(surface) - 2 * s) / cell(surface) - 2).max(0) as usize
}

/// The text rows a prompt may take on `surface` beside its field, when it
/// has one, and its buttons, so those stay on the surface; never fewer
/// than one for the title and one for the instruction.
pub fn prompt_text_rows(surface: Surface, pin: bool) -> usize {
    let s = surface.scale.value() as i64;
    let rows = ((surface.height as i64) - 2 * s) / row(surface) - if pin { 2 } else { 1 };
    usize::try_from(rows).unwrap_or(0).max(2)
}

/// The prompt: `title_rows` naming the operation, `line_rows` of
/// instruction, the PIN's field when it asks one, and its buttons.
pub fn prompt(surface: Surface, pin: bool, title_rows: usize, line_rows: usize) -> Prompt {
    let row = row(surface);
    let cell = cell(surface);
    // The border's width; the rows lie inside it.
    let s = surface.scale.value() as i64;
    let rows_of = |rows: usize| i64::try_from(rows.max(1)).unwrap_or(1);
    let (title_rows, line_rows) = (rows_of(title_rows), rows_of(line_rows));
    let field = title_rows + line_rows;
    let rows = field + if pin { 2 } else { 1 };
    let width = prompt_width(surface);
    let height = rows * row + 2 * s;
    let x = ((surface.width as i64) - width) / 2;
    let y = (((surface.height as i64) - height) / 2).max(0);
    let (inside, top) = (width - 2 * s, y + s);
    Prompt {
        rect: rect(x, y, width, height),
        title: rect(x + s, top, inside, title_rows * row),
        line: rect(x + s, top + title_rows * row, inside, line_rows * row),
        pin: TextEntry::new(
            surface,
            rect(x + cell, top + field * row, width - 2 * cell, row),
        )
        .filter(|_| pin),
        buttons: Buttons::in_band(
            surface,
            x + s,
            top + (rows - 1) * row,
            inside.max(0) as u32,
            if pin { &PIN } else { &PRESENT },
        ),
    }
}

/// A question's rows: a title, a few details and its actions.
pub const DIALOG_ROWS: i64 = 7;
/// The create question's rows: its title, every detail's wrapped rows at
/// the dialog's widest and its three actions, so what one key risks shows
/// without scrolling.
pub const CREATE_ROWS: i64 = 13;
/// The swap question's rows: at 800x600 the body's height, so all it says
/// shows without scrolling.
pub const SWAP_ROWS: i64 = 22;

/// Where the confirmation dialog may go, in the order to try it: centred,
/// then at the top and the bottom of the body, so its actions can be kept
/// from under the pointer that opened it.
pub fn dialog(surface: Surface, rows: i64) -> [Rect; 3] {
    let body = body(surface, &NOTEBOOK);
    let row = row(surface);
    let cell = cell(surface);
    let width = ((surface.width as i64) - 2 * cell).clamp(0, 56 * cell);
    let height = rows.saturating_mul(row).min(i64::from(body.height));
    let x = ((surface.width as i64) - width) / 2;
    let centre = body.y + (i64::from(body.height) - height) / 2;
    let bottom = body.y + i64::from(body.height) - height;
    [centre, body.y, bottom].map(|y| rect(x, y, width, height))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use td_ui::raster::Scale;

    #[test]
    fn the_panes_share_the_body_without_overlap() {
        for scale in [1, 2] {
            let s = i64::from(scale);
            let surface = Surface::new(
                800 * scale as usize,
                600 * scale as usize,
                Scale::new(scale).unwrap(),
            )
            .unwrap();
            let body = body(surface, &NOTEBOOK);
            let split = panes(surface, true);
            let search = split.search.unwrap().rect();
            let list = split.list.unwrap().rect();
            let title = split.title.unwrap().rect();
            let find = split.find.unwrap().rect();
            assert_eq!(search.y, body.y);
            assert_eq!(list.y, search.y + i64::from(search.height));
            assert_eq!(
                list.y + i64::from(list.height),
                body.y + i64::from(body.height)
            );
            // The right side begins where the left ends and reaches the
            // body's right; each side's outline is its own.
            assert_eq!(search.x + i64::from(search.width), title.x);
            let frame = split.frame;
            assert_eq!((frame.x, frame.width), (title.x, title.width));
            assert_eq!(frame.x + i64::from(frame.width), i64::from(body.width));
            assert_eq!(frame.y, title.y + i64::from(title.height));
            assert_eq!(find.y, frame.y + i64::from(frame.height));
            // The pane lies a scaled pixel inside its outline on every side.
            let pane = split.pane;
            assert_eq!((pane.x, pane.y), (frame.x + s, frame.y + s));
            assert_eq!(
                (i64::from(pane.width), i64::from(pane.height)),
                (
                    i64::from(frame.width) - 2 * s,
                    i64::from(frame.height) - 2 * s
                )
            );
            let closed = panes(surface, false);
            assert!(closed.find.is_none());
            assert_eq!(
                closed.frame.y + i64::from(closed.frame.height),
                body.y + i64::from(body.height)
            );
        }
    }

    #[test]
    fn the_prompt_and_dialog_fit_the_surface() {
        let surface = Surface::new(320, 240, Scale::default()).unwrap();
        for (pin, title_rows, line_rows) in [(true, 1, 1), (false, 1, 1), (true, 2, 3)] {
            let prompt = prompt(surface, pin, title_rows, line_rows);
            assert_eq!(prompt.pin.is_some(), pin);
            assert_eq!(
                prompt.rect.intersection(surface.bounds()),
                Some(prompt.rect)
            );
            // Every row lies inside the border.
            let inside = Rect {
                x: prompt.rect.x + 1,
                y: prompt.rect.y + 1,
                width: prompt.rect.width - 2,
                height: prompt.rect.height - 2,
            };
            for row in [prompt.title, prompt.line, prompt.buttons.rect()] {
                assert_eq!(row.intersection(inside), Some(row), "{row:?}");
            }
        }
        for rect in dialog(surface, DIALOG_ROWS) {
            assert_eq!(rect.intersection(surface.bounds()), Some(rect));
        }
    }
}
