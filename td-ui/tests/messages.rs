#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The message list's oracles: wrapping, collapsing, scrolling and paging,
//! selection across messages, Shift extension, word selection, chrome kept
//! out of every copy, whole-message and tool-result copies through the
//! window's clipboard, the bounds, the geometry and draw-stream and pixel
//! checks at scales one through four.

use std::sync::Arc;

use td_ui::chrome::{ROW, SELECTED_ROW};
use td_ui::messages::{
    Controller, Error, Event, Key, Message, Outcome, Point, Shown, Tone, COPY_LABEL, EXCERPT_ROWS,
    GAP, MAX_LABEL_BYTES, MAX_LINES, MAX_SECTIONS, MAX_TEXT_BYTES, MAX_TOTAL_BYTES, MORE,
};
use td_ui::raster::{
    Composition, Draw, Primitive, Raster, Rect, Scale, Surface, BORDER, CHROME, INACTIVE_SELECTION,
    INK, LINE_NUMBER, PAPER, SELECTED,
};
use td_ui::window::{Clipboard, Refusal};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

/// The window's clipboard as a test sees it: every copy recorded, or
/// refused as asked.
#[derive(Default)]
struct Board {
    copies: Vec<String>,
    refuse: Option<Refusal>,
}

impl Clipboard for Board {
    fn available(&self) -> bool {
        true
    }
    fn has_text(&self) -> bool {
        false
    }
    fn pasting(&self) -> bool {
        false
    }
    fn copy(&mut self, text: Arc<str>) -> Result<(), Refusal> {
        if let Some(refusal) = self.refuse {
            return Err(refusal);
        }
        self.copies.push(text.to_string());
        Ok(())
    }
    fn paste(&mut self) -> Result<(), Refusal> {
        Err(Refusal::NoDevice)
    }
}

fn surface(scale: u8) -> Surface {
    let s = usize::from(scale);
    Surface::new(640 * s, 400 * s, Scale::new(scale).unwrap()).unwrap()
}

/// A rectangle of `columns` text columns (a cell each side and the
/// scrollbar gutter beside them) and `height` font pixels.
fn rect(scale: u8, columns: u32, height: u32) -> Rect {
    let s = u32::from(scale);
    Rect {
        x: 16 * i64::from(s),
        y: 24 * i64::from(s),
        width: ((columns + 2) * CELL_WIDTH as u32 + 16) * s,
        height: height * s,
    }
}

fn list(scale: u8, columns: u32, height: u32) -> Controller {
    Controller::new(surface(scale), rect(scale, columns, height)).unwrap()
}

fn text(header: &str, body: &str) -> Message {
    Message::new(header).unwrap().text(body).unwrap()
}

/// The shown rows, spelled `H0` (header), `T0.1+`/`T0.1-` (an open or
/// collapsed title), `M0.1` (an excerpt's more row), `G0` (the gap) and
/// the text of a text row.
fn rows(list: &Controller) -> Vec<String> {
    list.shown()
        .map(|(_, shown)| match shown {
            Shown::Header { message } => format!("H{message}"),
            Shown::Title {
                message,
                section,
                collapsed,
            } => format!("T{message}.{section}{}", if collapsed { '-' } else { '+' }),
            Shown::Text { text, .. } => text.to_string(),
            Shown::More { message, section } => format!("M{message}.{section}"),
            Shown::Gap { message } => format!("G{message}"),
        })
        .collect()
}

/// The rectangle of the `nth` shown text row.
fn text_row(list: &Controller, nth: usize) -> Rect {
    list.shown()
        .filter(|(_, shown)| matches!(shown, Shown::Text { .. }))
        .nth(nth)
        .unwrap()
        .0
}

/// A point inside column `column` of a shown row, just right of its edge.
fn at(row: Rect, column: usize, scale: u8) -> (i64, i64) {
    let s = i64::from(scale);
    (
        row.x + (1 + column as i64) * CELL_WIDTH as i64 * s + s,
        row.y + 2 * s,
    )
}

fn press(
    list: &mut Controller,
    board: &mut Board,
    (x, y): (i64, i64),
    extend: bool,
    at_ms: u64,
) -> Outcome {
    list.event(
        Event::Press {
            x,
            y,
            extend,
            at_ms,
        },
        board,
    )
}

fn release(list: &mut Controller, board: &mut Board, (x, y): (i64, i64), at_ms: u64) -> Outcome {
    list.event(Event::Release { x, y, at_ms }, board)
}

fn key(list: &mut Controller, board: &mut Board, key: Key) -> Outcome {
    list.event(Event::Key { key, repeat: false }, board)
}

#[test]
fn bodies_wrap_to_the_width_at_every_scale() {
    for scale in 1..=4 {
        let mut list = list(scale, 20, 300);
        list.push(text(
            "user",
            "the quick brown fox jumps over the lazy dog\nabcdefghijklmnopqrstuvwxyz",
        ))
        .unwrap();
        assert_eq!(
            rows(&list),
            [
                "H0",
                "the quick brown fox ",
                "jumps over the lazy ",
                "dog",
                "abcdefghijklmnopqrst",
                "uvwxyz",
                "G0"
            ],
            "scale {scale}"
        );
        // Header, text and gap heights, scaled, stacked from the top.
        let rects: Vec<Rect> = list.shown().map(|(rect, _)| rect).collect();
        let s = u32::from(scale);
        assert_eq!(rects[0].y, list.rect().y);
        assert_eq!(rects[0].height, ROW as u32 * s);
        assert_eq!(rects[1].height, CELL_HEIGHT as u32 * s);
        assert_eq!(rects[6].height, GAP as u32 * s);
        for pair in rects.windows(2) {
            assert_eq!(pair[0].y + i64::from(pair[0].height), pair[1].y);
        }
    }
}

#[test]
fn rows_tile_the_text_and_keep_a_word_whole() {
    let mut list = list(1, 20, 300);
    list.push(text("a", "aaaaaaaaaaaaaaaaaaaa bbb")).unwrap();
    list.push(text("b", "")).unwrap();
    list.push(text("c", "x\n")).unwrap();
    list.push(text("d", "one two\tthree")).unwrap();
    assert_eq!(
        rows(&list),
        [
            "H0",
            // The space past the last column stays on its row, unshown.
            "aaaaaaaaaaaaaaaaaaaa ",
            "bbb",
            "G0",
            "H1",
            "",
            "G1",
            "H2",
            "x",
            "",
            "G2",
            "H3",
            "one two\tthree",
            "G3"
        ]
    );
}

#[test]
fn messages_and_titled_sections_collapse_to_one_row() {
    let mut board = Board::default();
    let mut list = list(1, 30, 300);
    list.push(
        Message::new("assistant")
            .unwrap()
            .section("reasoning", "think hard", true)
            .unwrap()
            .text("the answer")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(rows(&list), ["H0", "T0.0-", "the answer", "G0"]);
    // Hidden text is not selected.
    key(&mut list, &mut board, Key::SelectAll);
    assert_eq!(list.selected_text().unwrap().as_deref(), Some("the answer"));
    // A press on the title row opens it.
    let title = list.shown().nth(1).unwrap().0;
    assert_eq!(
        press(&mut list, &mut board, (title.x + 40, title.y + 2), false, 0),
        Outcome::Changed
    );
    assert_eq!(
        rows(&list),
        ["H0", "T0.0+", "think hard", "the answer", "G0"]
    );
    key(&mut list, &mut board, Key::SelectAll);
    assert_eq!(
        list.selected_text().unwrap().as_deref(),
        Some("think hard\n\nthe answer")
    );
    // The header's mark collapses the message to its header row.
    let header = list.shown().next().unwrap().0;
    assert_eq!(
        press(
            &mut list,
            &mut board,
            (header.x + 10, header.y + 5),
            false,
            0
        ),
        Outcome::Changed
    );
    assert_eq!(rows(&list), ["H0", "G0"]);
    assert!(list.message(0).unwrap().is_collapsed());
    assert_eq!(list.focused_message(), Some(0));
    // Toggle opens the focused message again; an untitled section has no
    // title to collapse.
    assert_eq!(key(&mut list, &mut board, Key::Toggle), Outcome::Changed);
    assert_eq!(
        rows(&list),
        ["H0", "T0.0+", "think hard", "the answer", "G0"]
    );
    assert_eq!(
        list.set_section_collapsed(0, 1, true),
        Err(Error::NoSection)
    );
    assert_eq!(list.set_section_collapsed(0, 0, true), Ok(true));
    assert_eq!(list.set_section_collapsed(0, 0, true), Ok(false));
    assert_eq!(rows(&list), ["H0", "T0.0-", "the answer", "G0"]);
    // A repeated Toggle does nothing.
    assert_eq!(
        list.event(
            Event::Key {
                key: Key::Toggle,
                repeat: true
            },
            &mut board
        ),
        Outcome::Consumed
    );
}

/// Thirty one-line messages: each a header, a text row and a gap, 48 font
/// pixels, five to a 240-pixel view.
fn long(scale: u8) -> Controller {
    let mut list = list(scale, 30, 240);
    for n in 0..30 {
        list.push(text("user", &format!("line {n}"))).unwrap();
    }
    list
}

fn first(list: &Controller) -> String {
    rows(list).into_iter().next().unwrap()
}

#[test]
fn the_list_scrolls_pages_and_follows_its_end() {
    for scale in 1..=4 {
        let mut board = Board::default();
        let mut list = long(scale);
        // Following, the last message ends the view.
        assert!(list.following());
        assert_eq!(rows(&list).last().unwrap(), "G29");
        assert_eq!(rows(&list).len(), 15);
        assert_eq!(key(&mut list, &mut board, Key::End), Outcome::Consumed);
        assert_eq!(key(&mut list, &mut board, Key::Home), Outcome::Changed);
        assert!(!list.following());
        assert_eq!(first(&list), "H0");
        assert_eq!(key(&mut list, &mut board, Key::Up), Outcome::Consumed);
        assert_eq!(key(&mut list, &mut board, Key::Down), Outcome::Changed);
        assert_eq!(first(&list), "line 0");
        // A page moves to the first row the view did not hold whole.
        key(&mut list, &mut board, Key::PageDown);
        assert_eq!(first(&list), "line 5");
        key(&mut list, &mut board, Key::PageUp);
        assert_eq!(first(&list), "line 0");
        list.event(Event::Wheel { rows: 4 }, &mut board);
        assert_eq!(first(&list), "G1");
        list.event(Event::Wheel { rows: -2 }, &mut board);
        assert_eq!(first(&list), "H1");
        // A message arriving while scrolled back leaves the view alone.
        list.push(text("user", "line 30")).unwrap();
        assert_eq!(first(&list), "H1");
        assert!(!list.following());
        // Back at the end, the view follows what arrives.
        assert_eq!(key(&mut list, &mut board, Key::End), Outcome::Changed);
        assert!(list.following());
        list.push(text("user", "line 31")).unwrap();
        assert_eq!(rows(&list).last().unwrap(), "G31");
        list.append(31, 0, "\nand more").unwrap();
        assert_eq!(rows(&list).last().unwrap(), "G31");
        assert_eq!(rows(&list)[rows(&list).len() - 2], "and more");
    }
}

#[test]
fn a_resize_keeps_the_first_shown_row() {
    let mut board = Board::default();
    let mut list = long(1);
    key(&mut list, &mut board, Key::Home);
    for _ in 0..7 {
        key(&mut list, &mut board, Key::Down);
    }
    assert_eq!(first(&list), "line 2");
    list.resize(surface(2), rect(2, 24, 200)).unwrap();
    assert_eq!(first(&list), "line 2");
    list.resize(surface(1), rect(1, 40, 240)).unwrap();
    assert_eq!(first(&list), "line 2");
    // Dropping older messages keeps the row in view under its new index.
    list.remove_first(1);
    assert_eq!(first(&list), "line 2");
    assert_eq!(list.len(), 29);
    list.remove_first(5);
    assert_eq!(first(&list), "H0");
    assert_eq!(rows(&list)[1], "line 6");
}

#[test]
fn a_drag_past_the_view_scrolls_it_a_row_a_motion() {
    let mut board = Board::default();
    let mut list = long(1);
    key(&mut list, &mut board, Key::Home);
    let start = at(text_row(&list, 0), 0, 1);
    press(&mut list, &mut board, start, false, 0);
    let below = list.rect().y + i64::from(list.rect().height) + 5;
    for _ in 0..4 {
        assert_eq!(
            list.event(Event::Move { x: 0, y: below }, &mut board),
            Outcome::Changed
        );
    }
    assert_eq!(first(&list), "line 1");
    // The head at the first shown row's start, the second message's...
    list.event(
        Event::Move {
            x: 0,
            y: list.rect().y,
        },
        &mut board,
    );
    // ...is its header's point too, so a scroll up to the header moves
    // the view and not the head, and still asks for a repaint.
    let head = list.selection().unwrap().1;
    let above = list.rect().y - 5;
    assert_eq!(
        list.event(Event::Move { x: 0, y: above }, &mut board),
        Outcome::Changed
    );
    assert_eq!(first(&list), "H1");
    assert_eq!(list.selection().unwrap().1, head);
    assert_eq!(list.selected_text().unwrap().as_deref(), Some("line 0"));
    list.event(Event::Move { x: 0, y: above }, &mut board);
    assert_eq!(first(&list), "G0");
    list.event(Event::Cancel, &mut board);
    assert_eq!(
        list.event(Event::Move { x: 0, y: below }, &mut board),
        Outcome::Ignored
    );
}

#[test]
fn moving_between_messages_focuses_and_reveals_them() {
    let mut board = Board::default();
    let mut list = long(1);
    key(&mut list, &mut board, Key::Home);
    assert_eq!(
        key(&mut list, &mut board, Key::NextMessage),
        Outcome::Changed
    );
    assert_eq!(list.focused_message(), Some(0));
    for _ in 0..6 {
        key(&mut list, &mut board, Key::NextMessage);
    }
    assert_eq!(list.focused_message(), Some(6));
    assert!(rows(&list).contains(&"H6".to_string()));
    key(&mut list, &mut board, Key::Home);
    key(&mut list, &mut board, Key::PreviousMessage);
    assert_eq!(list.focused_message(), Some(5));
    assert_eq!(first(&list), "H5");
    // Toggle folds the focused message and shows its header too.
    key(&mut list, &mut board, Key::End);
    assert!(!rows(&list).contains(&"H5".to_string()));
    assert_eq!(key(&mut list, &mut board, Key::Toggle), Outcome::Changed);
    assert!(list.message(5).unwrap().is_collapsed());
    assert_eq!(first(&list), "H5");
}

/// A user message and an assistant message with a status and a verdict.
fn pair(scale: u8) -> Controller {
    let mut list = list(scale, 50, 300);
    list.push(text("user", "alpha beta")).unwrap();
    list.push(
        Message::new("assistant")
            .unwrap()
            .text("gamma delta")
            .unwrap()
            .status("done", Tone::Neutral)
            .unwrap()
            .verdict("allowed", Tone::Good)
            .unwrap(),
    )
    .unwrap();
    list
}

#[test]
fn a_drag_selects_across_messages_without_their_chrome() {
    for scale in 1..=4 {
        let mut board = Board::default();
        let mut list = pair(scale);
        let from = at(text_row(&list, 0), 6, scale);
        let to = at(text_row(&list, 1), 6, scale);
        assert_eq!(
            press(&mut list, &mut board, from, false, 0),
            Outcome::Changed
        );
        // The drag passes over the second message's header.
        let header = list.shown().nth(3).unwrap().0;
        list.event(
            Event::Move {
                x: from.0,
                y: header.y + 3,
            },
            &mut board,
        );
        assert_eq!(
            list.selected_text().unwrap().as_deref(),
            Some("beta"),
            "a header is its message's start"
        );
        list.event(Event::Move { x: to.0, y: to.1 }, &mut board);
        assert_eq!(release(&mut list, &mut board, to, 10), Outcome::Consumed);
        assert_eq!(
            list.selection(),
            Some((
                Point {
                    message: 0,
                    section: 0,
                    byte: 6
                },
                Point {
                    message: 1,
                    section: 0,
                    byte: 6
                }
            ))
        );
        assert_eq!(key(&mut list, &mut board, Key::Copy), Outcome::Copied);
        assert_eq!(board.copies, ["beta\n\ngamma "]);
        // A drag past the view's foot runs to the text's end.
        press(&mut list, &mut board, from, false, 2000);
        list.event(Event::Move { x: 0, y: i64::MAX }, &mut board);
        assert_eq!(
            list.selected_text().unwrap().as_deref(),
            Some("beta\n\ngamma delta")
        );
    }
}

#[test]
fn shift_extends_the_selection_from_its_anchor() {
    let mut board = Board::default();
    let mut list = pair(1);
    let start = at(text_row(&list, 0), 0, 1);
    press(&mut list, &mut board, start, false, 0);
    release(&mut list, &mut board, start, 5);
    let end = at(text_row(&list, 1), 5, 1);
    assert_eq!(
        press(&mut list, &mut board, end, true, 2000),
        Outcome::Changed
    );
    release(&mut list, &mut board, end, 2010);
    assert_eq!(
        list.selected_text().unwrap().as_deref(),
        Some("alpha beta\n\ngamma")
    );
    // Shift again moves the head, not the anchor.
    let back = at(text_row(&list, 0), 6, 1);
    press(&mut list, &mut board, back, true, 4000);
    assert_eq!(list.selected_text().unwrap().as_deref(), Some("alpha "));
}

#[test]
fn a_double_click_selects_a_word() {
    let mut board = Board::default();
    let mut list = pair(2);
    let beta = at(text_row(&list, 0), 7, 2);
    press(&mut list, &mut board, beta, false, 1000);
    release(&mut list, &mut board, beta, 1050);
    assert_eq!(
        press(&mut list, &mut board, (beta.0 + 3, beta.1), false, 1300),
        Outcome::Changed
    );
    assert_eq!(list.selected_text().unwrap().as_deref(), Some("beta"));
    // Its drag does not undo the word, and the third press is a first.
    list.event(Event::Move { x: 0, y: 0 }, &mut board);
    release(&mut list, &mut board, beta, 1350);
    assert_eq!(list.selected_text().unwrap().as_deref(), Some("beta"));
    press(&mut list, &mut board, beta, false, 1400);
    assert_eq!(list.selected_text().unwrap(), None);
    release(&mut list, &mut board, beta, 1410);
    // Too slow, or too far, is two single clicks.
    press(&mut list, &mut board, beta, false, 2000);
    assert_eq!(list.selected_text().unwrap(), None);
    release(&mut list, &mut board, beta, 2010);
    press(&mut list, &mut board, (beta.0 + 40, beta.1), false, 2100);
    assert_eq!(list.selected_text().unwrap(), None);
    // A word wrapped across rows is selected whole.
    let mut list = crate::list(1, 20, 300);
    list.push(text("user", "see abcdefghijklmnopqrstuvwxyz now"))
        .unwrap();
    let second = at(text_row(&list, 1), 2, 1);
    press(&mut list, &mut board, second, false, 0);
    release(&mut list, &mut board, second, 10);
    press(&mut list, &mut board, second, false, 20);
    assert_eq!(
        list.selected_text().unwrap().as_deref(),
        Some("abcdefghijklmnopqrstuvwxyz")
    );
}

/// A tool block: its arguments, a twenty-line excerpt of its result and
/// the whole result as its copy source, after a reasoned reply.
fn transcript() -> Controller {
    let mut list = list(1, 40, 360);
    list.push(
        Message::new("assistant")
            .unwrap()
            .section("reasoning", "first, look", false)
            .unwrap()
            .text("Reading the file.")
            .unwrap(),
    )
    .unwrap();
    let excerpt: Vec<String> = (0..20).map(|n| format!("row {n}")).collect();
    let whole = format!("{}\nrow 20\nrow 21", excerpt.join("\n"));
    list.push(
        Message::new("tool read_file")
            .unwrap()
            .section("arguments", "{\"path\":\"a.txt\"}", false)
            .unwrap()
            .excerpt("result", &excerpt.join("\n"))
            .unwrap()
            .status("succeeded", Tone::Good)
            .unwrap()
            .verdict("allowed", Tone::Good)
            .unwrap()
            .source(Arc::from(whole))
            .unwrap(),
    )
    .unwrap();
    list
}

#[test]
fn an_excerpt_shows_its_bound_and_chrome_is_never_selected() {
    let mut board = Board::default();
    let mut list = transcript();
    let shown = rows(&list);
    assert_eq!(
        shown,
        [
            "H0",
            "T0.0+",
            "first, look",
            "Reading the file.",
            "G0",
            "H1",
            "T1.0+",
            "{\"path\":\"a.txt\"}",
            "T1.1+",
            "row 0",
            "row 1",
            "row 2",
            "row 3",
            "row 4",
            "row 5",
            "row 6",
            "row 7",
            "M1.1",
            "G1"
        ]
    );
    assert_eq!(EXCERPT_ROWS, 8);
    key(&mut list, &mut board, Key::SelectAll);
    assert_eq!(key(&mut list, &mut board, Key::Copy), Outcome::Copied);
    let copied = board.copies.pop().unwrap();
    assert_eq!(
        copied,
        "first, look\n\nReading the file.\n\n{\"path\":\"a.txt\"}\n\nrow 0\nrow 1\nrow 2\nrow 3\nrow 4\nrow 5\nrow 6\nrow 7"
    );
    for chrome in [
        "assistant",
        "reasoning",
        "tool read_file",
        "arguments",
        "result",
        "succeeded",
        "allowed",
        COPY_LABEL,
        MORE,
        "\u{2713}",
    ] {
        assert!(!copied.contains(chrome), "{chrome} in the copy");
    }
    // A press on a header or a title starts no selection to drag.
    let header = list.shown().nth(5).unwrap().0;
    press(
        &mut list,
        &mut board,
        (header.x + 60, header.y + 4),
        false,
        0,
    );
    assert_eq!(
        list.event(Event::Move { x: 0, y: i64::MAX }, &mut board),
        Outcome::Ignored
    );
    // A drag from the excerpt's last row over its more row and the gap
    // ends at the shown text.
    let last = at(text_row(&list, 10), 4, 1);
    press(&mut list, &mut board, last, false, 3000);
    list.event(Event::Move { x: 0, y: i64::MAX }, &mut board);
    assert_eq!(list.selected_text().unwrap().as_deref(), Some("7"));
}

#[test]
fn every_message_copies_whole_from_its_header_or_its_key() {
    for scale in 1..=4 {
        let mut board = Board::default();
        let mut list = transcript();
        list.resize(surface(scale), {
            let mut r = rect(scale, 40, 380);
            r.y = 8 * i64::from(scale);
            r
        })
        .unwrap();
        // The header's button copies the message's source text: every
        // section whole, at the press.
        let button = list.copy_button(0).unwrap();
        assert_eq!(
            press(
                &mut list,
                &mut board,
                (button.x + 2, button.y + 2),
                false,
                0
            ),
            Outcome::Copied
        );
        assert_eq!(board.copies, ["first, look\n\nReading the file."]);
        assert_eq!(list.focused_message(), Some(0));
        // The release after it is nothing.
        assert_eq!(
            release(&mut list, &mut board, (button.x + 2, button.y + 2), 5),
            Outcome::Ignored
        );
        // The tool block copies its whole result, not the excerpt.
        key(&mut list, &mut board, Key::NextMessage);
        assert_eq!(
            key(&mut list, &mut board, Key::CopyMessage),
            Outcome::Copied
        );
        let whole = board.copies.pop().unwrap();
        assert!(whole.ends_with("row 19\nrow 20\nrow 21"));
        assert_eq!(whole.lines().count(), 22);
        assert_eq!(&*list.message_text(1).unwrap(), whole);
        // A collapsed message copies whole as well.
        list.set_collapsed(0, true).unwrap();
        assert_eq!(
            &*list.message_text(0).unwrap(),
            "first, look\n\nReading the file."
        );
    }
}

#[test]
fn copies_are_refused_or_empty_as_the_clipboard_and_selection_say() {
    let mut board = Board::default();
    let mut list = pair(1);
    assert_eq!(
        key(&mut list, &mut board, Key::Copy),
        Outcome::NothingToCopy
    );
    assert_eq!(
        key(&mut list, &mut board, Key::CopyMessage),
        Outcome::NothingToCopy
    );
    let start = at(text_row(&list, 0), 0, 1);
    press(&mut list, &mut board, start, false, 0);
    release(&mut list, &mut board, start, 5);
    assert_eq!(
        key(&mut list, &mut board, Key::Copy),
        Outcome::NothingToCopy
    );
    key(&mut list, &mut board, Key::SelectAll);
    board.refuse = Some(Refusal::NoSerial);
    assert_eq!(
        key(&mut list, &mut board, Key::Copy),
        Outcome::Refused(Refusal::NoSerial)
    );
    board.refuse = None;
    // A repeat never copies.
    assert_eq!(
        list.event(
            Event::Key {
                key: Key::Copy,
                repeat: true
            },
            &mut board
        ),
        Outcome::Consumed
    );
    assert!(board.copies.is_empty());
    assert_eq!(Key::from_chord("C-c"), Some(Key::Copy));
    assert_eq!(Key::from_chord("C-S-c"), Some(Key::CopyMessage));
    assert_eq!(Key::from_chord("C-a"), Some(Key::SelectAll));
    assert_eq!(Key::from_chord("PageDown"), Some(Key::PageDown));
    assert_eq!(Key::from_chord("x"), None);
}

#[test]
fn streaming_text_keeps_the_selection() {
    let mut board = Board::default();
    let mut list = pair(1);
    key(&mut list, &mut board, Key::SelectAll);
    list.append(1, 0, " epsilon").unwrap();
    assert_eq!(
        list.selected_text().unwrap().as_deref(),
        Some("alpha beta\n\ngamma delta")
    );
    assert_eq!(rows(&list)[4], "gamma delta epsilon");
    list.set_status(1, Some(("failed", Tone::Bad))).unwrap();
    list.set_verdict(1, None).unwrap();
    // Replacing a message the selection reaches clears it.
    list.replace(1, text("assistant", "new")).unwrap();
    assert_eq!(list.selection(), None);
    assert_eq!(list.append(2, 0, "x"), Err(Error::NoMessage));
    assert_eq!(list.append(1, 1, "x"), Err(Error::NoSection));
}

#[test]
fn messages_and_the_list_are_bounded() {
    assert_eq!(Message::new("").unwrap_err(), Error::Empty);
    assert_eq!(Message::new("a\tb").unwrap_err(), Error::Control);
    assert_eq!(
        Message::new(&"a".repeat(MAX_LABEL_BYTES + 1)).unwrap_err(),
        Error::Limit
    );
    assert!(Message::new(&"a".repeat(MAX_LABEL_BYTES)).is_ok());
    let big = "a".repeat(MAX_TEXT_BYTES);
    assert!(Message::new("m").unwrap().text(&big).is_ok());
    assert_eq!(
        Message::new("m")
            .unwrap()
            .text(&format!("{big}a"))
            .unwrap_err(),
        Error::Limit
    );
    assert_eq!(
        Message::new("m")
            .unwrap()
            .source(Arc::from(format!("{big}a")))
            .unwrap_err(),
        Error::Limit
    );
    let mut message = Message::new("m").unwrap();
    for _ in 0..MAX_SECTIONS {
        message = message.text("x").unwrap();
    }
    assert_eq!(message.text("x").unwrap_err(), Error::Limit);
    assert_eq!(
        Message::new("m")
            .unwrap()
            .status("", Tone::Good)
            .unwrap_err(),
        Error::Empty
    );
    assert_eq!(
        Message::new("m")
            .unwrap()
            .section("t\n", "x", false)
            .unwrap_err(),
        Error::Control
    );
    // The whole list's text is bounded, and freed when messages go.
    let mut list = list(1, 60, 300);
    let full = MAX_TOTAL_BYTES / MAX_TEXT_BYTES;
    for _ in 0..full - 1 {
        list.push(Message::new("m").unwrap().text(&big).unwrap())
            .unwrap();
    }
    assert_eq!(
        list.push(Message::new("m").unwrap().text(&big).unwrap()),
        Err(Error::Limit)
    );
    assert_eq!(list.len(), full - 1);
    list.push(text("m", "small")).unwrap();
    assert_eq!(list.append(0, 0, "a"), Err(Error::Limit));
    let before = list.storage_bytes();
    list.remove_first(1);
    assert_eq!(list.storage_bytes(), before - MAX_TEXT_BYTES - 1);
    list.append(0, 0, "").unwrap();
}

#[test]
fn a_rectangle_too_small_lays_nothing_out_until_one_holds_it() {
    let mut board = Board::default();
    let tiny = Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    };
    let mut list = Controller::new(surface(1), tiny).unwrap();
    list.push(text("user", "hello")).unwrap();
    assert!(!list.has_layout());
    assert_eq!(list.shown().count(), 0);
    let mut draws = 0;
    list.emit(surface(1).bounds(), &mut |_| draws += 1);
    assert_eq!(draws, 0);
    assert_eq!(
        press(&mut list, &mut board, (5, 5), false, 0),
        Outcome::Ignored
    );
    list.resize(surface(1), rect(1, 30, 200)).unwrap();
    assert!(list.has_layout());
    assert_eq!(rows(&list), ["H0", "hello", "G0"]);
    let outside = Rect {
        x: 600,
        ..rect(1, 30, 200)
    };
    assert_eq!(list.resize(surface(1), outside), Err(Error::InvalidRect));
    assert!(!list.has_layout());
    assert_eq!(
        Controller::new(surface(1), outside).unwrap_err(),
        Error::InvalidRect
    );
}

struct Scene<'a> {
    surface: Surface,
    list: &'a Controller,
}

impl Composition for Scene<'_> {
    fn surface(&self) -> Surface {
        self.surface
    }
    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        self.list.emit(damage, sink)
    }
}

#[test]
fn the_draw_stream_stays_inside_and_names_the_chrome() {
    for scale in 1..=4 {
        let surface = surface(scale);
        let list = pair(scale);
        let mut draws = Vec::new();
        list.emit(surface.bounds(), &mut |draw| draws.push(draw));
        let bounds = list.rect();
        let glyphs: String = draws
            .iter()
            .filter_map(|draw| match draw.primitive {
                Primitive::Glyph { scalar, .. } => Some(scalar),
                _ => None,
            })
            .collect();
        for draw in &draws {
            assert_eq!(draw.clip.intersection(bounds), Some(draw.clip));
        }
        for expected in [
            "user",
            "assistant",
            "done",
            "\u{2713} allowed",
            "alpha beta",
            "Copy",
        ] {
            assert!(glyphs.contains(expected), "{expected} at scale {scale}");
        }
        // The body's first glyph sits a cell into its row, in ink on paper.
        let row = text_row(&list, 0);
        let s = i64::from(scale);
        assert!(draws.iter().any(|draw| draw.primitive
            == Primitive::Glyph {
                x: row.x + CELL_WIDTH as i64 * s,
                y: row.y,
                scalar: 'a',
                style: td_ui::raster::GlyphStyle::medium(INK, PAPER),
            }));
        // Damage off the list paints nothing.
        let mut none = 0;
        list.emit(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            &mut |_| none += 1,
        );
        assert_eq!(none, 0);
    }
}

fn pixel(buffer: &[u8], surface: Surface, x: i64, y: i64) -> u32 {
    let at = (y as usize * surface.width + x as usize) * 4;
    u32::from_le_bytes(buffer[at..at + 4].try_into().unwrap()) & 0xffffff
}

#[test]
fn pixels_land_inside_the_list_and_partial_repaints_match() {
    let font = td_ui::font::pinned().unwrap();
    for scale in 1..=4 {
        let s = i64::from(scale);
        let surface = surface(scale);
        let mut board = Board::default();
        let mut list = pair(scale);
        // Select "beta" in the focused list and focus the second message.
        let from = at(text_row(&list, 0), 6, scale);
        let to = at(text_row(&list, 0), 10, scale);
        press(&mut list, &mut board, from, false, 0);
        release(&mut list, &mut board, to, 5);
        list.event(Event::Focus(true), &mut board);
        key(&mut list, &mut board, Key::NextMessage);
        let paint = |list: &Controller, damage: Rect| {
            let mut buffer = vec![0x7a; surface.width * surface.height * 4];
            let scene = Scene { surface, list };
            Raster::new(&mut buffer, &font, surface, surface.width * 4)
                .unwrap()
                .paint(&scene, damage)
                .unwrap();
            buffer
        };
        let whole = paint(&list, surface.bounds());
        let bounds = list.rect();
        for y in 0..surface.height as i64 {
            for x in 0..surface.width as i64 {
                if !bounds.contains(x, y) {
                    assert_eq!(pixel(&whole, surface, x, y), 0x7a7a7a);
                }
            }
        }
        let shown: Vec<(Rect, Shown)> = list.shown().collect();
        let (first_header, second_header) = (shown[0].0, shown[3].0);
        // Each header has a rule on top, then its ground: chrome, or the
        // focused message's highlight.
        assert_eq!(
            pixel(&whole, surface, first_header.x + 2, first_header.y),
            BORDER
        );
        assert_eq!(
            pixel(&whole, surface, first_header.x + 2, first_header.y + 2 * s),
            CHROME
        );
        assert_eq!(
            pixel(
                &whole,
                surface,
                second_header.x + 2,
                second_header.y + 2 * s
            ),
            SELECTED_ROW & 0xffffff
        );
        // The copy button's bezel.
        let button = list.copy_button(0).unwrap();
        assert_eq!(pixel(&whole, surface, button.x, button.y), BORDER);
        // The selected cells' ground, under a focused list, and paper
        // beside them.
        let row = text_row(&list, 0);
        let cell = CELL_WIDTH as i64 * s;
        assert_eq!(pixel(&whole, surface, row.x + 7 * cell, row.y), SELECTED);
        assert_eq!(pixel(&whole, surface, row.x + 12 * cell, row.y), PAPER);
        // Unfocused, the selection greys.
        list.event(Event::Focus(false), &mut board);
        let unfocused = paint(&list, surface.bounds());
        assert_eq!(
            pixel(&unfocused, surface, row.x + 7 * cell, row.y),
            INACTIVE_SELECTION
        );
        // A partial repaint equals the whole inside its damage and leaves
        // everything else.
        let damage = Rect {
            x: row.x + 3 * s,
            y: first_header.y + 5 * s,
            width: 90 * scale as u32,
            height: 40 * scale as u32,
        };
        let partial = paint(&list, damage);
        for y in 0..surface.height as i64 {
            for x in 0..surface.width as i64 {
                let expected = if damage.contains(x, y) {
                    pixel(&unfocused, surface, x, y)
                } else {
                    0x7a7a7a
                };
                assert_eq!(pixel(&partial, surface, x, y), expected, "({x}, {y})");
            }
        }
    }
}

/// Each shown text row's start, length and rectangle in a one-section
/// message's text: rows tile it, a newline after a row belonging to none.
fn starts(list: &Controller, text: &str) -> Vec<(usize, usize, Rect)> {
    let mut at = 0;
    list.shown()
        .filter_map(|(rect, shown)| match shown {
            Shown::Text { text: row, .. } => Some((rect, row.len())),
            _ => None,
        })
        .map(|(rect, len)| {
            let start = at;
            at += len;
            if text.as_bytes().get(at) == Some(&b'\n') {
                at += 1;
            }
            (start, len, rect)
        })
        .collect()
}

#[test]
fn a_selection_copies_exactly_the_source_between_its_ends() {
    let text = "\nalpha\n\nbeta gamma delta epsilon\n";
    let mut board = Board::default();
    let mut list = list(1, 16, 300);
    list.push(text_message(text)).unwrap();
    assert_eq!(
        rows(&list),
        [
            "H0",
            "",
            "alpha",
            "",
            "beta gamma delta ",
            "epsilon",
            "",
            "G0"
        ]
    );
    let shown = starts(&list, text);
    // Every press and drag between two places of the shown rows, the
    // empty rows and the ends of rows among them; a row's unshown space
    // past the sixteenth column is no place a press reaches.
    let mut at_ms = 0;
    for &(start, len, rect) in &shown {
        for column in 0..=len.min(16) {
            for &(to_start, to_len, to_rect) in &shown {
                for to_column in 0..=to_len.min(16) {
                    at_ms += 1000;
                    press(&mut list, &mut board, at(rect, column, 1), false, at_ms);
                    let (x, y) = at(to_rect, to_column, 1);
                    list.event(Event::Move { x, y }, &mut board);
                    release(&mut list, &mut board, (x, y), at_ms + 1);
                    let (a, b) = (start + column, to_start + to_column);
                    let expected = &text[a.min(b)..a.max(b)];
                    assert_eq!(
                        list.selected_text().unwrap().as_deref(),
                        (!expected.is_empty()).then_some(expected),
                        "{a}..{b}"
                    );
                }
            }
        }
    }
    key(&mut list, &mut board, Key::SelectAll);
    assert_eq!(list.selected_text().unwrap().as_deref(), Some(text));
    // Across messages: from the first's last row end nothing of it, from
    // within its last line that line's rest, its newline and a blank line.
    list.push(text_message("b")).unwrap();
    let last = shown.last().unwrap().2;
    let b = text_row(&list, shown.len());
    press(&mut list, &mut board, at(last, 0, 1), false, at_ms + 5000);
    list.event(
        Event::Move {
            x: at(b, 1, 1).0,
            y: b.y + 2,
        },
        &mut board,
    );
    assert_eq!(list.selected_text().unwrap().as_deref(), Some("b"));
    let epsilon = shown[4].2;
    press(
        &mut list,
        &mut board,
        at(epsilon, 7, 1),
        false,
        at_ms + 10000,
    );
    list.event(
        Event::Move {
            x: at(b, 1, 1).0,
            y: b.y + 2,
        },
        &mut board,
    );
    assert_eq!(list.selected_text().unwrap().as_deref(), Some("\n\n\nb"));
}

#[test]
fn an_empty_section_inside_a_selection_keeps_its_place() {
    let mut board = Board::default();
    let mut list = list(1, 30, 300);
    list.push(
        Message::new("user")
            .unwrap()
            .text("a")
            .unwrap()
            .text("")
            .unwrap()
            .text("b")
            .unwrap(),
    )
    .unwrap();
    key(&mut list, &mut board, Key::SelectAll);
    // The same text the whole-message copy gives.
    assert_eq!(list.selected_text().unwrap().as_deref(), Some("a\n\n\n\nb"));
    assert_eq!(&*list.message_text(0).unwrap(), "a\n\n\n\nb");
    // A selection ending at the empty section takes nothing of it.
    let a = at(text_row(&list, 0), 0, 1);
    let empty = at(text_row(&list, 1), 0, 1);
    press(&mut list, &mut board, a, false, 0);
    list.event(
        Event::Move {
            x: empty.0,
            y: empty.1,
        },
        &mut board,
    );
    assert_eq!(list.selected_text().unwrap().as_deref(), Some("a"));
}

fn text_message(body: &str) -> Message {
    text("user", body)
}

#[test]
fn a_space_past_the_last_column_ends_the_text_without_an_empty_row() {
    let mut list = list(1, 20, 300);
    list.push(text_message("aaaaaaaaaaaaaaaaaaaa ")).unwrap();
    list.push(text_message("aaaaaaaaaaaaaaaaaaaa \n")).unwrap();
    assert_eq!(
        rows(&list),
        [
            "H0",
            "aaaaaaaaaaaaaaaaaaaa ",
            "G0",
            "H1",
            "aaaaaaaaaaaaaaaaaaaa ",
            "",
            "G1"
        ]
    );
}

#[test]
fn a_tab_draws_as_a_space_and_another_control_as_the_replacement() {
    let mut list = list(1, 20, 300);
    list.push(text_message("a\u{1b}b\tc")).unwrap();
    let row = text_row(&list, 0);
    let mut glyphs = Vec::new();
    list.emit(row, &mut |draw| {
        if let Primitive::Glyph { scalar, .. } = draw.primitive {
            glyphs.push(scalar);
        }
    });
    assert_eq!(glyphs, ['a', '\u{fffd}', 'b', ' ', 'c']);
}

#[test]
fn the_first_shown_row_survives_a_rectangle_too_small_for_any() {
    let mut board = Board::default();
    let mut list = long(1);
    key(&mut list, &mut board, Key::Home);
    for _ in 0..7 {
        key(&mut list, &mut board, Key::Down);
    }
    let tiny = Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    };
    list.resize(surface(1), tiny).unwrap();
    assert!(!list.has_layout());
    list.push(text_message("late")).unwrap();
    list.resize(surface(1), rect(1, 30, 240)).unwrap();
    assert_eq!(first(&list), "line 2");
    assert!(!list.following());
}

#[test]
fn a_view_one_header_tall_shows_the_header_and_not_its_gap_alone() {
    let mut list = list(1, 30, ROW as u32);
    list.push(text_message("x").collapsed(true)).unwrap();
    assert_eq!(rows(&list), ["H0"]);
    assert!(list.following());
    list.push(text_message("y").collapsed(true)).unwrap();
    assert_eq!(rows(&list), ["H1"]);
}

#[test]
fn a_trim_is_never_refused_and_lays_out_what_fits() {
    let tiny = Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    };
    let mut list = Controller::new(surface(1), tiny).unwrap();
    let lines = "\n".repeat(400_000);
    for _ in 0..3 {
        list.push(text_message(&lines)).unwrap();
    }
    assert_eq!(list.resize(surface(1), rect(1, 30, 200)), Err(Error::Limit));
    assert!(!list.has_layout());
    list.remove_first(1);
    assert_eq!(list.len(), 2);
    assert!(list.has_layout());
    // A list that lays out stays laid out through a trim.
    list.remove_first(1);
    assert_eq!(list.len(), 1);
    assert!(list.has_layout());
}

#[test]
fn layout_rows_are_bounded_and_a_refused_edit_leaves_the_list() {
    let mut list = list(1, 30, 200);
    assert_eq!(
        list.push(text_message(&"\n".repeat(MAX_LINES))),
        Err(Error::Limit)
    );
    assert!(list.is_empty());
    assert_eq!(list.storage_bytes(), 0);
    list.push(text_message(&"\n".repeat(MAX_LINES - 10)))
        .unwrap();
    let bytes = list.storage_bytes();
    assert_eq!(list.append(0, 0, &"\n".repeat(20)), Err(Error::Limit));
    assert_eq!(
        list.message(0).unwrap().section_text(0).unwrap().len(),
        MAX_LINES - 10
    );
    assert_eq!(list.storage_bytes(), bytes);
    list.append(0, 0, "\n\n\n").unwrap();
    assert_eq!(list.storage_bytes(), bytes + 3);
}

#[test]
fn copies_past_the_clipboard_ceiling_are_refused_as_too_long() {
    let mut board = Board::default();
    let mut list = list(1, 30, 200);
    let big = "a".repeat(MAX_TEXT_BYTES);
    list.push(
        Message::new("assistant")
            .unwrap()
            .text(&big)
            .unwrap()
            .text(&big)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(list.message_text(0), Err(Error::Limit));
    key(&mut list, &mut board, Key::NextMessage);
    assert_eq!(
        key(&mut list, &mut board, Key::CopyMessage),
        Outcome::Refused(Refusal::TooLong)
    );
    key(&mut list, &mut board, Key::SelectAll);
    assert_eq!(
        key(&mut list, &mut board, Key::Copy),
        Outcome::Refused(Refusal::TooLong)
    );
    assert!(board.copies.is_empty());
}

#[test]
fn a_double_click_past_a_row_or_into_hidden_text_takes_no_more() {
    let mut board = Board::default();
    let mut list = list(1, 20, 360);
    list.push(text_message("aaaa bbbbbbbbbbbbbbbbbbbb"))
        .unwrap();
    assert_eq!(rows(&list)[1..3], ["aaaa ", "bbbbbbbbbbbbbbbbbbbb"]);
    let past = at(text_row(&list, 0), 12, 1);
    press(&mut list, &mut board, past, false, 0);
    release(&mut list, &mut board, past, 10);
    press(&mut list, &mut board, past, false, 20);
    assert_eq!(list.selected_text().unwrap(), None);
    release(&mut list, &mut board, past, 30);
    // An excerpt's last shown row holds the start of a word it hides the
    // rest of; the word stops at the shown text.
    let hidden = format!("{}{}", "x\n".repeat(EXCERPT_ROWS - 1), "w".repeat(24));
    list.push(
        Message::new("tool")
            .unwrap()
            .excerpt("result", &hidden)
            .unwrap(),
    )
    .unwrap();
    let last = text_row(&list, 2 + EXCERPT_ROWS - 1);
    let word = at(last, 2, 1);
    press(&mut list, &mut board, word, false, 1000);
    release(&mut list, &mut board, word, 1010);
    press(&mut list, &mut board, word, false, 1020);
    assert_eq!(
        list.selected_text().unwrap().as_deref(),
        Some("w".repeat(20).as_str())
    );
    // The word's end is the shown text's end, not the hidden word's.
    let shown_end = 2 * (EXCERPT_ROWS - 1) + 20;
    assert_eq!(
        list.selection(),
        Some((
            Point {
                message: 1,
                section: 0,
                byte: 2 * (EXCERPT_ROWS - 1)
            },
            Point {
                message: 1,
                section: 0,
                byte: shown_end
            }
        ))
    );
}

#[test]
fn a_fold_at_the_end_keeps_the_pressed_row_shown() {
    let mut board = Board::default();
    let mut list = list(1, 30, 120);
    let reasoning: Vec<String> = (0..40).map(|n| format!("r{n}")).collect();
    list.push(text_message("question")).unwrap();
    list.push(
        Message::new("assistant")
            .unwrap()
            .section("reasoning", &reasoning.join("\n"), true)
            .unwrap()
            .text("answer")
            .unwrap(),
    )
    .unwrap();
    assert!(list.following());
    let title = |list: &Controller| {
        list.shown()
            .find(|(_, shown)| matches!(shown, Shown::Title { .. }))
            .map(|(rect, _)| rect)
    };
    let shut = title(&list).unwrap();
    assert_eq!(
        press(&mut list, &mut board, (shut.x + 40, shut.y + 2), false, 0),
        Outcome::Changed
    );
    assert_eq!(title(&list).unwrap().y, shut.y);
    assert!(rows(&list).contains(&"r0".to_string()));
    assert!(!list.following());
}

#[test]
fn streamed_text_lays_out_as_the_whole_text_would() {
    let mut board = Board::default();
    let reply = "lorem ipsum dolor sit amet, consectetur\nadipiscing elit, sed do eiusmod tempor incididunt ut labore\n\net dolore magna aliqua   ut enim";
    let result: String = (0..14)
        .map(|n| format!("out {n} \u{e9}t\u{e9}\n"))
        .collect();
    // A reply, a tool's excerpt, and reasoning streamed into the first of
    // two sections, so the re-wrap passes a later titled section to find
    // its row.
    let build = |reply: &str, result: &str, reasoning: &str| {
        let mut list = crate::list(1, 18, 360);
        list.push(text_message(reply)).unwrap();
        list.push(
            Message::new("tool")
                .unwrap()
                .excerpt("result", result)
                .unwrap(),
        )
        .unwrap();
        list.push(
            Message::new("assistant")
                .unwrap()
                .section("reasoning", reasoning, false)
                .unwrap()
                .section("then", "so it is", false)
                .unwrap(),
        )
        .unwrap();
        list
    };
    let mut list = build("", "", "");
    let chunks = |text: &str| -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        chars.chunks(3).map(|c| c.iter().collect()).collect()
    };
    let (a, b) = (chunks(reply), chunks(&result));
    let mut shown = (String::new(), String::new(), String::new());
    for step in 0..a.len().max(b.len()) {
        if let Some(chunk) = a.get(step) {
            list.append(0, 0, chunk).unwrap();
            shown.0.push_str(chunk);
            list.append(2, 0, chunk).unwrap();
            shown.2.push_str(chunk);
        }
        if let Some(chunk) = b.get(step) {
            list.append(1, 0, chunk).unwrap();
            shown.1.push_str(chunk);
        }
        let mut whole = build(&shown.0, &shown.1, &shown.2);
        assert_eq!(rows(&list), rows(&whole), "end, step {step}");
        key(&mut list, &mut board, Key::Home);
        key(&mut whole, &mut board, Key::Home);
        assert_eq!(rows(&list), rows(&whole), "start, step {step}");
        key(&mut list, &mut board, Key::End);
    }
}

#[test]
fn a_trim_moves_the_selection_and_focus_with_their_messages() {
    let mut board = Board::default();
    let mut list = pair(1);
    list.push(text("user", "zeta eta")).unwrap();
    let from = at(text_row(&list, 1), 6, 1);
    let to = at(text_row(&list, 2), 4, 1);
    press(&mut list, &mut board, from, false, 0);
    list.event(Event::Move { x: to.0, y: to.1 }, &mut board);
    release(&mut list, &mut board, to, 5);
    assert_eq!(list.focused_message(), Some(1));
    let before = list.selected_text().unwrap();
    assert_eq!(before.as_deref(), Some("delta\n\nzeta"));
    list.remove_first(1);
    assert_eq!(list.focused_message(), Some(0));
    assert_eq!(list.selected_text().unwrap(), before);
    assert_eq!(list.selection().unwrap().0.message, 0);
    list.remove_first(1);
    assert_eq!(list.focused_message(), None);
    assert_eq!(list.selection(), None);
}

#[test]
fn the_scrollbar_thumb_tracks_the_view() {
    let mut board = Board::default();
    let thumb = |list: &Controller| {
        let rect = list.rect();
        let gutter = rect.x + i64::from(rect.width) - 16;
        let mut found = None;
        list.emit(rect, &mut |draw| {
            if let Primitive::Fill { rect: fill, color } = draw.primitive {
                if fill.x == gutter {
                    found = Some((fill, color));
                }
            }
        });
        found.unwrap()
    };
    let mut list = long(1);
    let rect = list.rect();
    let (end, color) = thumb(&list);
    assert_eq!(color, LINE_NUMBER);
    assert_eq!(
        end.y + i64::from(end.height),
        rect.y + i64::from(rect.height)
    );
    key(&mut list, &mut board, Key::Home);
    let (start, _) = thumb(&list);
    assert_eq!(start.y, rect.y);
    assert_eq!(start.height, end.height);
    let still = pair(1);
    let (whole, color) = thumb(&still);
    assert_eq!(color, BORDER);
    assert_eq!(whole.height, still.rect().height);
}
