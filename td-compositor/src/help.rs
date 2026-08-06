use crate::ui;

const CARD_WIDTH: usize = 620;
const CARD_PADDING: usize = 24;
const TITLE_TOP: usize = 18;
const FIRST_ROW_TOP: usize = 60;
const ROW_STEP: usize = 26;
const BOTTOM_PADDING: usize = 22;
const KEYS_LEFT: usize = 20;
const ACTION_LEFT: usize = 280;
const SCALE: usize = 2;

/// One line of the cheat sheet, PAINTED as written. `input.rs` drives each
/// row's real chord and derives both columns back, since nothing the
/// compiler sees connects these strings to the dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Row {
    pub keys: &'static str,
    pub action: &'static str,
}

pub const ROWS: &[Row] = &[
    Row {
        keys: "SUPER+ARROWS",
        action: "FOCUS A TILE",
    },
    Row {
        keys: "SUPER+SHIFT+ARROWS",
        action: "MOVE A TILE",
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
        action: "SPLIT VERTICAL",
    },
    Row {
        keys: "SUPER+H",
        action: "SPLIT HORIZONTAL",
    },
    Row {
        keys: "SUPER+F",
        action: "TOGGLE FULLSCREEN",
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
    // Not a chord, and the only line here the dispatch test cannot drive. It
    // earns its place because a cheat sheet that omits the mouse leaves the
    // operator believing the keyboard is the only way to focus.
    Row {
        keys: "CLICK",
        action: "FOCUS A TILE",
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

#[derive(Clone, Default)]
pub struct Help {
    visible: bool,
}

impl Help {
    pub fn set(&mut self, visible: bool) {
        self.visible = visible;
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn paint(&self, frame: &mut [u8], width: usize, height: usize, stride: usize) {
        if !self.visible {
            return;
        }
        let card_width = CARD_WIDTH.min(width.saturating_sub(CARD_PADDING.saturating_mul(2)));
        let card_height = card_height().min(height.saturating_sub(CARD_PADDING.saturating_mul(2)));
        let left = width.saturating_sub(card_width) / 2;
        let top = height.saturating_sub(card_height) / 2;
        let card = (left, top, card_width, card_height);
        ui::fill(frame, width, height, stride, card, [0x18, 0x20, 0x28, 0]);
        ui::border(frame, width, height, stride, card, [0x70, 0xc0, 0xf0, 0]);
        ui::draw_text_clipped(
            frame,
            width,
            height,
            stride,
            left.saturating_add(KEYS_LEFT),
            top.saturating_add(TITLE_TOP),
            SCALE,
            "TD KEY BINDINGS",
            [0xff, 0xff, 0xff, 0],
            card,
        );
        for (index, row) in ROWS.iter().enumerate() {
            let row_top = top
                .saturating_add(FIRST_ROW_TOP)
                .saturating_add(index.saturating_mul(ROW_STEP));
            ui::draw_text_clipped(
                frame,
                width,
                height,
                stride,
                left.saturating_add(KEYS_LEFT),
                row_top,
                SCALE,
                row.keys,
                [0xb0, 0xd8, 0xf0, 0],
                card,
            );
            ui::draw_text_clipped(
                frame,
                width,
                height,
                stride,
                left.saturating_add(ACTION_LEFT),
                row_top,
                SCALE,
                row.action,
                [0xff, 0xff, 0xff, 0],
                card,
            );
        }
    }
}

/// Sized from the table rather than pinned, so adding a row cannot silently
/// push the last one past the card's bottom edge.
fn card_height() -> usize {
    FIRST_ROW_TOP
        .saturating_add(ROWS.len().saturating_mul(ROW_STEP))
        .saturating_add(BOTTOM_PADDING)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_width(text: &str) -> usize {
        text.len().saturating_mul(ui::GLYPH_ADVANCE).saturating_mul(SCALE)
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
    fn every_row_fits_its_column_and_the_card() {
        for row in ROWS {
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
            .saturating_add(ROWS.len().saturating_sub(1).saturating_mul(ROW_STEP))
            .saturating_add(ui::GLYPH_HEIGHT.saturating_mul(SCALE));
        assert!(last <= card_height(), "{last} rows past {}", card_height());
    }

    #[test]
    fn a_hidden_sheet_paints_nothing_and_a_visible_one_paints_inside_its_card() {
        let (width, height) = (900usize, 600usize);
        let stride = width.saturating_mul(4);
        let mut frame = vec![0u8; stride.saturating_mul(height)];
        let mut help = Help::default();
        help.paint(&mut frame, width, height, stride);
        assert!(frame.iter().all(|byte| *byte == 0));

        help.set(true);
        help.paint(&mut frame, width, height, stride);
        let card_width = CARD_WIDTH;
        let card_height = card_height();
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
        help.paint(&mut frame, width, height, stride);
        let card_width = CARD_WIDTH.min(width.saturating_sub(CARD_PADDING.saturating_mul(2)));
        let card_height = card_height().min(height.saturating_sub(CARD_PADDING.saturating_mul(2)));
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
