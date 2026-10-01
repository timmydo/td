//! A list's selection and scroll state for `chrome::List`: the item count,
//! the selected item and the first shown one. The consumer owns its items,
//! its key bindings and what activation means; `Step::from_chord` is the
//! default set. Geometry is passed in on each call, since a resize changes
//! the rows a list shows. The selection is a position: when the items
//! change, the consumer says which position its selected item now holds.

use crate::chrome::{Item, List};
use crate::raster::{Draw, Rect};
use std::ops::Range;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    Up,
    Down,
    /// By the rows the list shows at once.
    PageUp,
    PageDown,
    Home,
    End,
}

impl Step {
    pub fn from_chord(chord: &str) -> Option<Self> {
        match chord {
            "Up" => Some(Self::Up),
            "Down" => Some(Self::Down),
            "PageUp" => Some(Self::PageUp),
            "PageDown" => Some(Self::PageDown),
            "Home" => Some(Self::Home),
            "End" => Some(Self::End),
            _ => None,
        }
    }
}

/// What a call changed: the selected position, the shown window, or both.
/// Either one needs a repaint.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Change {
    pub selection: bool,
    pub window: bool,
}

impl Change {
    pub fn any(self) -> bool {
        self.selection || self.window
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ListModel {
    count: usize,
    selected: Option<usize>,
    first: usize,
    margin: usize,
}

impl ListModel {
    /// An empty list whose selection keeps `margin` rows shown on each side
    /// where the list has them, as `List::reveal_within` clamps it.
    pub fn with_margin(margin: usize) -> Self {
        Self {
            margin,
            ..Self::default()
        }
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn first(&self) -> usize {
        self.first
    }

    /// The item indices `list` shows.
    pub fn window(&self, list: List) -> Range<usize> {
        self.first..self.first.saturating_add(list.rows()).min(self.count)
    }

    /// Shows the selection, or keeps the window inside the items when there
    /// is none.
    fn reveal(&mut self, list: List, selection: bool) -> Change {
        let first = match self.selected {
            Some(selected) => list.reveal_within(self.count, selected, self.first, self.margin),
            None => self.first.min(self.count.saturating_sub(list.rows())),
        };
        let window = first != self.first;
        self.first = first;
        Change { selection, window }
    }

    /// Takes new items, as a filter or a reload gives, with the position
    /// the selected item now holds, or `None` when it is gone. A position
    /// past the end selects nothing.
    pub fn set_items(&mut self, count: usize, selected: Option<usize>, list: List) -> Change {
        let before = self.selected;
        self.count = count;
        self.selected = selected.filter(|&index| index < count);
        self.reveal(list, self.selected != before)
    }

    /// Selects `index`, or nothing, and shows it; an index past the end is
    /// refused and changes nothing.
    pub fn select(&mut self, index: Option<usize>, list: List) -> Change {
        if index.is_some_and(|index| index >= self.count) {
            return Change::default();
        }
        let selection = index != self.selected;
        self.selected = index;
        self.reveal(list, selection)
    }

    /// Moves the selection without wrapping and shows it, also at an end it
    /// already holds. With nothing selected `End` selects the last item and
    /// every other step the first.
    pub fn step(&mut self, step: Step, list: List) -> Change {
        let Some(last) = self.count.checked_sub(1) else {
            return Change::default();
        };
        let page = list.rows().max(1);
        let next = match (self.selected, step) {
            (None, Step::End) => last,
            (None, _) => 0,
            (Some(at), Step::Up) => at.saturating_sub(1),
            (Some(at), Step::Down) => (at + 1).min(last),
            (Some(at), Step::PageUp) => at.saturating_sub(page),
            (Some(at), Step::PageDown) => at.saturating_add(page).min(last),
            (Some(_), Step::Home) => 0,
            (Some(_), Step::End) => last,
        };
        self.select(Some(next), list)
    }

    /// Selects the item under a pointer point in `list` and returns it with
    /// what that changed; `None` off the items. The consumer decides what
    /// a double press activates.
    pub fn press(&mut self, list: List, x: i64, y: i64) -> Option<(usize, Change)> {
        let index = self.first.checked_add(list.hit(x, y)?)?;
        if index >= self.count {
            return None;
        }
        Some((index, self.select(Some(index), list)))
    }

    /// Scrolls by `rows`, negative up, leaving the selection where it is.
    pub fn scroll(&mut self, rows: i64, list: List) -> Change {
        let last_page = self.count.saturating_sub(list.rows());
        let magnitude = usize::try_from(rows.unsigned_abs()).unwrap_or(usize::MAX);
        let first = if rows < 0 {
            self.first.saturating_sub(magnitude)
        } else {
            self.first.saturating_add(magnitude)
        }
        .min(last_page);
        let window = first != self.first;
        self.first = first;
        Change {
            selection: false,
            window,
        }
    }

    /// Keeps the selection shown after `list` changed size.
    pub fn relayout(&mut self, list: List) -> Change {
        self.reveal(list, false)
    }

    /// Paints `items`, the window `window` names in order, through
    /// `List::emit` with this state; with nothing selected no row is
    /// highlighted.
    pub fn emit<'a>(
        &self,
        list: List,
        items: impl IntoIterator<Item = Item<'a>>,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        // `List::emit` highlights row `i` when `selected - first == i`;
        // `usize::MAX` is past every shown row.
        list.emit(
            items,
            self.first,
            self.selected.unwrap_or(usize::MAX),
            self.count,
            damage,
            sink,
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::raster::{Scale, Surface};

    /// A list showing `rows` rows.
    fn list(rows: usize) -> List {
        let surface = Surface::new(200, 400, Scale::new(1).unwrap()).unwrap();
        let rect = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: (24 * rows) as u32,
        };
        let list = List::new(surface, rect).unwrap();
        assert_eq!(list.rows(), rows);
        list
    }

    fn filled(count: usize, list: List) -> ListModel {
        let mut model = ListModel::default();
        model.set_items(count, None, list);
        model
    }

    const SELECTED: Change = Change {
        selection: true,
        window: false,
    };
    const BOTH: Change = Change {
        selection: true,
        window: true,
    };
    const WINDOW: Change = Change {
        selection: false,
        window: true,
    };
    const NOTHING: Change = Change {
        selection: false,
        window: false,
    };

    #[test]
    fn the_default_chords_name_the_steps() {
        assert_eq!(Step::from_chord("Up"), Some(Step::Up));
        assert_eq!(Step::from_chord("PageDown"), Some(Step::PageDown));
        assert_eq!(Step::from_chord("End"), Some(Step::End));
        assert_eq!(Step::from_chord("S-Up"), None);
        assert_eq!(Step::from_chord("Return"), None);
    }

    #[test]
    fn steps_move_without_wrapping_and_keep_the_selection_shown() {
        let list = list(4);
        let mut model = filled(10, list);
        assert_eq!(model.selected(), None);
        assert_eq!(model.step(Step::Down, list), SELECTED);
        assert_eq!(model.selected(), Some(0));
        assert_eq!(model.step(Step::Up, list), NOTHING);
        assert_eq!(model.step(Step::Home, list), NOTHING);
        assert_eq!(model.step(Step::PageDown, list), BOTH);
        assert_eq!((model.selected(), model.first()), (Some(4), 1));
        assert_eq!(model.step(Step::End, list), BOTH);
        assert_eq!((model.selected(), model.first()), (Some(9), 6));
        assert_eq!(model.step(Step::Down, list), NOTHING);
        assert_eq!(model.step(Step::PageDown, list), NOTHING);
        assert_eq!(model.window(list), 6..10);
        assert_eq!(model.step(Step::PageUp, list), BOTH);
        assert_eq!((model.selected(), model.first()), (Some(5), 5));

        let mut fresh = filled(10, list);
        assert_eq!(fresh.step(Step::End, list), BOTH);
        assert_eq!(fresh.selected(), Some(9));
        let mut empty = filled(0, list);
        assert_eq!(empty.step(Step::Down, list), NOTHING);
        assert_eq!((empty.selected(), empty.window(list)), (None, 0..0));
    }

    #[test]
    fn a_step_at_an_end_shows_a_selection_scrolled_away() {
        let list = list(4);
        let mut model = filled(10, list);
        model.step(Step::Home, list);
        assert_eq!(model.scroll(6, list), WINDOW);
        assert_eq!(model.step(Step::Home, list), WINDOW);
        assert_eq!((model.selected(), model.first()), (Some(0), 0));
        model.step(Step::End, list);
        model.scroll(-6, list);
        assert_eq!(model.step(Step::Down, list), WINDOW);
        assert_eq!(model.first(), 6);
        model.scroll(-6, list);
        assert_eq!(model.select(Some(9), list), WINDOW);
    }

    #[test]
    fn new_items_take_the_selection_the_consumer_names() {
        let list = list(4);
        let mut model = filled(10, list);
        model.select(Some(8), list);
        assert_eq!(model.first(), 5);
        // A filter keeping the selected item at a new position.
        assert_eq!(model.set_items(5, Some(2), list), BOTH);
        assert_eq!((model.selected(), model.first()), (Some(2), 1));
        // The same position after a reload is no selection change.
        assert_eq!(model.set_items(12, Some(2), list), NOTHING);
        // A filter dropping it, or naming a position past the end.
        assert_eq!(model.set_items(3, None, list), BOTH);
        assert_eq!(model.selected(), None);
        assert_eq!(model.set_items(3, Some(3), list), NOTHING);
        assert_eq!(model.selected(), None);

        assert_eq!(model.set_items(5, None, list), NOTHING);
        assert_eq!(model.select(Some(5), list), NOTHING);
        assert_eq!(model.select(Some(4), list), BOTH);
        assert_eq!(model.select(Some(4), list), NOTHING);
        assert_eq!(model.select(None, list), SELECTED);
        assert_eq!(model.first(), 1);
        // Shrinking with nothing selected still pulls the window in.
        assert_eq!(model.set_items(4, None, list), WINDOW);
        assert_eq!(model.first(), 0);
        assert_eq!(model.set_items(0, None, list), NOTHING);
        assert_eq!(model.window(list), 0..0);
    }

    #[test]
    fn a_margin_keeps_rows_shown_around_the_selection() {
        let list = list(5);
        let mut model = ListModel::with_margin(1);
        model.set_items(20, None, list);
        model.select(Some(3), list);
        assert_eq!(model.first(), 0);
        model.step(Step::Down, list);
        assert_eq!((model.selected(), model.first()), (Some(4), 1));
        model.step(Step::Down, list);
        assert_eq!((model.selected(), model.first()), (Some(5), 2));
        model.step(Step::Up, list);
        model.step(Step::Up, list);
        assert_eq!((model.selected(), model.first()), (Some(3), 2));
        model.step(Step::End, list);
        assert_eq!(model.first(), 15);
        // A press on the last shown row scrolls to keep the margin.
        model.select(Some(2), list);
        assert_eq!(model.first(), 1);
        model.scroll(-1, list);
        assert_eq!(model.press(list, 5, 24 * 4 + 1), Some((4, BOTH)));
        assert_eq!(model.first(), 1);
    }

    #[test]
    fn press_and_scroll_go_through_the_shown_rows() {
        let list = list(4);
        let mut model = filled(6, list);
        assert_eq!(model.scroll(1, list), WINDOW);
        assert_eq!(model.scroll(100, list), WINDOW);
        assert_eq!(model.first(), 2);
        assert_eq!(model.scroll(1, list), NOTHING);
        assert_eq!(model.selected(), None);
        assert_eq!(model.press(list, 5, 24 + 3), Some((3, SELECTED)));
        assert_eq!(model.selected(), Some(3));
        assert_eq!(model.scroll(-1, list), WINDOW);
        assert_eq!(model.scroll(i64::MIN, list), WINDOW);
        assert_eq!((model.first(), model.selected()), (0, Some(3)));
        // Off the items, and in the scrollbar gutter, select nothing.
        let mut short = filled(2, list);
        assert_eq!(short.press(list, 5, 24 * 3 + 1), None);
        assert_eq!(short.press(list, 95, 1), None);
        assert_eq!(short.selected(), None);
        assert_eq!(model.press(list, 5, 1000), None);
    }

    #[test]
    fn a_resize_reveals_the_selection_again() {
        let tall = list(8);
        let short = list(2);
        let mut model = filled(10, tall);
        model.select(Some(7), tall);
        assert_eq!(model.first(), 0);
        assert_eq!(model.relayout(short), WINDOW);
        assert_eq!(model.first(), 6);
        assert_eq!(model.relayout(short), NOTHING);
    }

    #[test]
    fn emit_highlights_the_selection_and_nothing_without_one() {
        let list = list(3);
        let mut model = filled(5, list);
        let labels = ["a", "b", "c", "d", "e"];
        let items = |range: Range<usize>| {
            labels[range].iter().map(|label| Item {
                label,
                meta: "",
                enabled: true,
                marked: false,
            })
        };
        let damage = list.rect();
        let draws = |first: usize, selected: usize, range: Range<usize>| {
            let mut draws = Vec::new();
            list.emit(items(range), first, selected, 5, damage, &mut |d| {
                draws.push(d)
            });
            draws
        };
        let painted = |model: &ListModel| {
            let mut draws = Vec::new();
            model.emit(list, items(model.window(list)), damage, &mut |d| {
                draws.push(d)
            });
            draws
        };
        // Nothing selected paints as a selection outside the window does,
        // and differs from every shown row's highlight.
        let unselected = painted(&model);
        assert_eq!(unselected, draws(0, 4, 0..3));
        for row in 0..3 {
            assert_ne!(unselected, draws(0, row, 0..3));
        }

        model.select(Some(4), list);
        assert_eq!(painted(&model), draws(2, 4, 2..5));
    }
}
