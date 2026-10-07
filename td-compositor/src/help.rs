use crate::text::Text;
use crate::ui;

const CARD_WIDTH: usize = 620;
const CARD_PADDING: usize = 24;
const TITLE_TOP: usize = 18;
const FIRST_ROW_TOP: usize = 48;
const ROW_STEP: usize = 26;
const BOTTOM_PADDING: usize = 10;
const KEYS_LEFT: usize = 20;
const ACTION_LEFT: usize = 280;
const CARD: [u8; 4] = [0x18, 0x20, 0x28, 0];

/// One line of the cheat sheet, PAINTED as written. `input.rs` drives each
/// row's real chord and derives both columns back, since nothing the
/// compiler sees connects these strings to the dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Row {
    pub keys: &'static str,
    pub action: &'static str,
}

/// The chords every profile binds.
pub const CHORDS: &[Row] = &[
    Row {
        keys: "SUPER+ARROWS",
        action: "FOCUS A TILE",
    },
    Row {
        keys: "SUPER+SHIFT+ARROWS",
        action: "MOVE A TILE / SPLIT OUT",
    },
    Row {
        keys: "SUPER+1..9",
        action: "SWITCH WORKSPACE",
    },
    Row {
        keys: "SUPER+SHIFT+1..9",
        action: "MOVE TO WORKSPACE",
    },
    Row {
        keys: "SUPER+V",
        action: "STACK A COLUMN",
    },
    Row {
        keys: "SUPER+H",
        action: "TAB A COLUMN",
    },
    Row {
        keys: "SUPER+F",
        action: "TOGGLE FULLSCREEN",
    },
    Row {
        keys: "SUPER+S",
        action: "GROUP A COLUMN",
    },
    Row {
        keys: "SUPER+T",
        action: "NEW TERMINAL",
    },
    Row {
        keys: "SUPER+ENTER",
        action: "OPEN LAUNCHER",
    },
    Row {
        keys: "SUPER+?",
        action: "THIS HELP",
    },
];

/// `Super+l`'s row. Only the paired profile binds it
/// (td-login/TOKEN-LOGIN.md, "Session lock"), so only its sheet lists it,
/// below the other chords.
pub const LOCK: Row = Row {
    keys: "SUPER+L",
    action: "LOCK SCREEN",
};

/// The pointer's rows, below the chords.
pub const POINTING: &[Row] = &[
    // Not chords, and the only lines here the dispatch test cannot drive.
    // They earn their place because a cheat sheet that omits the mouse leaves
    // the operator believing the keyboard is the only way to focus — and HOVER
    // is now the mouse's primary way, with the click the one that works when
    // the pointer is already where it wants to be.
    Row {
        keys: "HOVER",
        action: "FOCUS A TILE",
    },
    Row {
        keys: "CLICK",
        action: "FOCUS A TILE",
    },
    Row {
        keys: "DRAG A TITLE",
        action: "MOVE A TILE / SPLIT OUT",
    },
    // The strip always shows one empty workspace, so this gesture always has a
    // target — but a drop zone nothing points at is invisible in a way a key
    // chord is not, since there is no key to press by accident and discover it.
    // Named as `Super+Shift+N` is: one effect, two ways to reach it, as HOVER
    // and CLICK already share their words.
    Row {
        keys: "DRAG TO THE BAR",
        action: "MOVE TO WORKSPACE",
    },
    // The bar's own two gestures, and the reason they are here is the reason
    // the drop above is: a control nothing points at is invisible in a way a
    // key chord is not. Two rows for one effect, as HOVER and CLICK already
    // are — the strip answers a press on a NUMBER and a notch anywhere on the
    // bar, which is one sentence too long for one row and two gestures an
    // operator reaches for at different moments.
    Row {
        keys: "CLICK THE BAR",
        action: "SWITCH WORKSPACE",
    },
    Row {
        keys: "SCROLL THE BAR",
        action: "SWITCH WORKSPACE",
    },
    // The button at the bar's left end: `Super+Enter` for the pointer, and
    // named here for the reason the strip's gestures are.
    Row {
        keys: "CLICK THE BAR MENU",
        action: "OPEN LAUNCHER",
    },
];

/// What the input layer asks of the sheet. `Close` is what a key press while
/// it is up always means: there is nothing to type into and nothing to
/// select, so a key can only mean "seen it".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelpAction {
    Toggle,
    Close,
}

impl HelpAction {
    /// The bit this asks for, given the bit now.
    pub fn target(self, visible: bool) -> bool {
        match self {
            HelpAction::Toggle => !visible,
            HelpAction::Close => false,
        }
    }
}

/// The sheet's rows in order: the chords, `Super+l`'s where `locks`, then
/// the pointer's.
pub fn rows(locks: bool) -> impl Iterator<Item = &'static Row> {
    CHORDS.iter().chain(locks.then_some(&LOCK)).chain(POINTING)
}

#[derive(Clone, Default)]
pub struct Help {
    visible: bool,
    /// The paired profile's sheet, which lists `Super+l`.
    locks: bool,
}

impl Help {
    pub fn set(&mut self, visible: bool) {
        self.visible = visible;
    }

    /// Whether the sheet lists `Super+l`, as the paired profile's does.
    pub fn list_lock(&mut self, locks: bool) {
        self.locks = locks;
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn paint(&self, frame: &mut [u8], width: usize, height: usize, stride: usize, text: &Text) {
        if !self.visible {
            return;
        }
        let card_width = CARD_WIDTH.min(width.saturating_sub(CARD_PADDING.saturating_mul(2)));
        let card_height =
            card_height(self.locks).min(height.saturating_sub(CARD_PADDING.saturating_mul(2)));
        let left = width.saturating_sub(card_width) / 2;
        let top = height.saturating_sub(card_height) / 2;
        let card = (left, top, card_width, card_height);
        ui::fill(frame, width, height, stride, card, CARD);
        ui::border(frame, width, height, stride, card, [0x70, 0xc0, 0xf0, 0]);
        text.draw(
            frame,
            width,
            height,
            stride,
            (
                left.saturating_add(KEYS_LEFT),
                top.saturating_add(TITLE_TOP),
            ),
            "TD KEY BINDINGS",
            ([0xff, 0xff, 0xff, 0], CARD),
            card,
        );
        for (index, row) in rows(self.locks).enumerate() {
            let row_top = top
                .saturating_add(FIRST_ROW_TOP)
                .saturating_add(index.saturating_mul(ROW_STEP));
            text.draw(
                frame,
                width,
                height,
                stride,
                (left.saturating_add(KEYS_LEFT), row_top),
                row.keys,
                ([0xb0, 0xd8, 0xf0, 0], CARD),
                card,
            );
            text.draw(
                frame,
                width,
                height,
                stride,
                (left.saturating_add(ACTION_LEFT), row_top),
                row.action,
                ([0xff, 0xff, 0xff, 0], CARD),
                card,
            );
        }
    }
}

/// Sized from the table rather than pinned, so adding a row cannot silently
/// push the last one past the card's bottom edge.
fn card_height(locks: bool) -> usize {
    FIRST_ROW_TOP
        .saturating_add(rows(locks).count().saturating_mul(ROW_STEP))
        .saturating_add(BOTTOM_PADDING)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_width(text: &str) -> usize {
        Text::width(text)
    }

    #[test]
    fn toggle_flips_the_bit_and_close_only_ever_clears_it() {
        for visible in [false, true] {
            assert_eq!(HelpAction::Toggle.target(visible), !visible);
            assert!(!HelpAction::Close.target(visible));
        }
        let mut help = Help::default();
        assert!(!help.visible());
        help.set(HelpAction::Toggle.target(help.visible()));
        assert!(help.visible());
        help.set(HelpAction::Close.target(help.visible()));
        assert!(!help.visible());
    }

    #[test]
    fn every_character_the_sheet_spells_is_in_the_font() {
        // `SUPER+1..9` once drew as `SUPER+1??9` for want of a period, so a
        // gap in the font was unreadable as one. Unifont is what draws a
        // character the outline face lacks, so it must have them all.
        for row in rows(true) {
            for character in row.keys.chars().chain(row.action.chars()) {
                assert!(
                    crate::text::covered(character),
                    "{character:?} in {:?} has no glyph",
                    row.keys
                );
            }
        }
    }

    #[test]
    fn every_row_fits_its_column_and_the_card() {
        for row in rows(true) {
            assert!(
                KEYS_LEFT.saturating_add(text_width(row.keys)) <= ACTION_LEFT,
                "{} overruns the action column",
                row.keys
            );
            assert!(
                ACTION_LEFT.saturating_add(text_width(row.action))
                    <= CARD_WIDTH.saturating_sub(KEYS_LEFT),
                "{} overruns the card",
                row.action
            );
        }
        let last = FIRST_ROW_TOP
            .saturating_add(
                rows(true)
                    .count()
                    .saturating_sub(1)
                    .saturating_mul(ROW_STEP),
            )
            .saturating_add(crate::text::CELL_HEIGHT);
        assert!(
            last <= card_height(true),
            "{last} rows past {}",
            card_height(true)
        );
        // And the card the rows are sized against fits a real screen, which
        // `card_height` alone does not say: `paint` CLIPS it to the output, so
        // a row past the bottom of one is a row that silently does not appear.
        // 768 is the ordinary virtio-gpu mode this crate is written against;
        // below it the sheet clips, which is defined and has its own test, but
        // a cheat sheet missing its last rows is a cheat sheet nobody can use.
        const ORDINARY_ROWS: usize = 768;
        assert!(
            card_height(true) <= ORDINARY_ROWS.saturating_sub(CARD_PADDING.saturating_mul(2)),
            "the card wants {} rows of a {ORDINARY_ROWS}-row output",
            card_height(true)
        );
        // The paired card, `Super+l`'s row included, fits 800x600 whole
        // too, and the title clears the first row by more than a row gap.
        assert_eq!(card_height(true), 552);
        assert!(card_height(true) <= 600 - CARD_PADDING * 2);
        const _: () = assert!(TITLE_TOP + ROW_STEP < FIRST_ROW_TOP);
    }

    /// Only the paired profile binds `Super+l`, so only its sheet lists it,
    /// once, right below the other chords, on a card one row taller.
    #[test]
    fn only_the_paired_sheet_lists_super_l() {
        let direct: Vec<&Row> = rows(false).collect();
        let paired: Vec<&Row> = rows(true).collect();
        assert!(!direct.contains(&&LOCK));
        assert_eq!(paired.len(), direct.len() + 1);
        assert_eq!(paired.get(CHORDS.len()), Some(&&LOCK));
        assert_eq!(paired.iter().filter(|row| ***row == LOCK).count(), 1);
        assert_eq!(card_height(true), card_height(false) + ROW_STEP);
        let (width, height) = (900usize, 700usize);
        let stride = width * 4;
        let painted = |locks: bool| {
            let mut frame = vec![0u8; stride * height];
            let mut help = Help::default();
            help.set(true);
            help.list_lock(locks);
            help.paint(&mut frame, width, height, stride, &Text::default());
            frame
        };
        assert!(painted(true) != painted(false));
    }

    #[test]
    fn a_hidden_sheet_paints_nothing_and_a_visible_one_paints_inside_its_card() {
        let (width, height) = (900usize, 600usize);
        let stride = width.saturating_mul(4);
        let mut frame = vec![0u8; stride.saturating_mul(height)];
        let mut help = Help::default();
        help.paint(&mut frame, width, height, stride, &Text::default());
        assert!(frame.iter().all(|byte| *byte == 0));

        help.set(true);
        help.paint(&mut frame, width, height, stride, &Text::default());
        let card_width = CARD_WIDTH;
        let card_height = card_height(false);
        let left = width.saturating_sub(card_width) / 2;
        let top = height.saturating_sub(card_height) / 2;
        let mut painted = 0usize;
        for y in 0..height {
            for x in 0..width {
                let offset = y.saturating_mul(stride).saturating_add(x.saturating_mul(4));
                let Some(pixel) = frame.get(offset..offset.saturating_add(4)) else {
                    continue;
                };
                if pixel.iter().any(|byte| *byte != 0) {
                    painted = painted.saturating_add(1);
                    assert!(
                        x >= left
                            && x < left.saturating_add(card_width)
                            && y >= top
                            && y < top.saturating_add(card_height),
                        "pixel at {x},{y} escaped the card"
                    );
                }
            }
        }
        assert!(painted > 0);
    }

    #[test]
    fn an_output_too_small_for_the_card_still_clips_every_pixel() {
        // The card is 620 wide and taller than this output; nothing may run
        // off the end of a row buffer or wrap onto the next line.
        let (width, height) = (200usize, 120usize);
        let stride = width.saturating_mul(4);
        let mut frame = vec![0u8; stride.saturating_mul(height)];
        let mut help = Help::default();
        help.set(true);
        help.paint(&mut frame, width, height, stride, &Text::default());
        let card_width = CARD_WIDTH.min(width.saturating_sub(CARD_PADDING.saturating_mul(2)));
        let card_height =
            card_height(false).min(height.saturating_sub(CARD_PADDING.saturating_mul(2)));
        let left = width.saturating_sub(card_width) / 2;
        let top = height.saturating_sub(card_height) / 2;
        for y in 0..height {
            for x in 0..width {
                let offset = y.saturating_mul(stride).saturating_add(x.saturating_mul(4));
                let Some(pixel) = frame.get(offset..offset.saturating_add(4)) else {
                    continue;
                };
                if pixel.iter().any(|byte| *byte != 0) {
                    assert!(
                        x >= left
                            && x < left.saturating_add(card_width)
                            && y >= top
                            && y < top.saturating_add(card_height),
                        "pixel at {x},{y} escaped the clipped card"
                    );
                }
            }
        }
    }
}
