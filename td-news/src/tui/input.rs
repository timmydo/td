//! The reader's input vocabulary, and how the window's arrives in it.
//!
//! The views were written against a terminal's keys and SGR mouse
//! reports; they keep that vocabulary, and the screen window's input
//! (`td_ui::screen::Input`) is translated into it here, so a click still
//! carries the terminal's one-based row and a wheel frame is one scroll
//! event per row of travel.

use td_ui::screen::{self, Input, Press};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    /// The row as a terminal reports it: one-based, the title row first.
    LeftClick {
        row: usize,
    },
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    Key(Key),
    Mouse(MouseEvent),
}

/// The most scroll events one wheel frame becomes, so a fling is bounded.
const WHEEL_EVENTS: usize = 64;

/// The key a press names, or none: a chord with Control, Alt or Shift is
/// not one of the reader's keys (the terminal read only the unmodified
/// key sequences; a shifted scalar arrives folded, with no shift), and
/// the vocabulary has no Escape, Tab or Delete.
pub fn key(press: Press) -> Option<Key> {
    if press.control || press.alt || press.shift {
        return None;
    }
    Some(match press.key {
        screen::Key::Char(c) => Key::Char(c),
        screen::Key::Enter => Key::Enter,
        screen::Key::Backspace => Key::Backspace,
        screen::Key::Up => Key::Up,
        screen::Key::Down => Key::Down,
        screen::Key::PageUp => Key::PageUp,
        screen::Key::PageDown => Key::PageDown,
        screen::Key::Home => Key::Home,
        screen::Key::End => Key::End,
        screen::Key::Left
        | screen::Key::Right
        | screen::Key::Escape
        | screen::Key::Tab
        | screen::Key::Insert
        | screen::Key::Delete
        | screen::Key::Function(_) => return None,
    })
}

/// The reader's events for one window input; `mouse` is the
/// configuration's switch, off meaning clicks and wheel travel are nothing.
pub fn translate(input: Input, mouse: bool, out: &mut Vec<InputEvent>) {
    match input {
        Input::Key(press) => {
            if let Some(key) = key(press) {
                out.push(InputEvent::Key(key));
            }
        }
        Input::Click { row, .. } if mouse => {
            out.push(InputEvent::Mouse(MouseEvent::LeftClick {
                row: row.saturating_add(1),
            }));
        }
        Input::Wheel { rows, .. } if mouse => {
            let event = if rows < 0 {
                MouseEvent::ScrollUp
            } else {
                MouseEvent::ScrollDown
            };
            let count = rows.unsigned_abs().min(WHEEL_EVENTS);
            out.extend(std::iter::repeat_n(InputEvent::Mouse(event), count));
        }
        Input::Click { .. }
        | Input::Wheel { .. }
        | Input::Resize { .. }
        | Input::Focus(_)
        | Input::Close => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presses_translate_to_the_readers_keys_and_chords_to_nothing() {
        let mut out = Vec::new();
        for (chord, expected) in [
            ("a", Some(Key::Char('a'))),
            ("Space", Some(Key::Char(' '))),
            ("Return", Some(Key::Enter)),
            ("Backspace", Some(Key::Backspace)),
            ("PageDown", Some(Key::PageDown)),
            ("End", Some(Key::End)),
            ("A", Some(Key::Char('A'))),
            ("C-c", None),
            ("M-Return", None),
            ("S-Up", None),
            ("S-PageDown", None),
            ("Escape", None),
            ("Tab", None),
            ("F1", None),
        ] {
            out.clear();
            let press = screen::press(chord).expect(chord);
            translate(Input::Key(press), true, &mut out);
            assert_eq!(
                out,
                expected
                    .map(InputEvent::Key)
                    .into_iter()
                    .collect::<Vec<_>>(),
                "{chord}"
            );
        }
    }

    #[test]
    fn clicks_carry_the_terminals_row_and_wheel_travel_is_one_event_per_row() {
        let mut out = Vec::new();
        translate(Input::Click { row: 2, column: 5 }, true, &mut out);
        assert_eq!(out, [InputEvent::Mouse(MouseEvent::LeftClick { row: 3 })]);
        out.clear();
        translate(
            Input::Wheel {
                rows: -2,
                columns: 0,
            },
            true,
            &mut out,
        );
        assert_eq!(out, [InputEvent::Mouse(MouseEvent::ScrollUp); 2]);
        out.clear();
        translate(
            Input::Wheel {
                rows: 3,
                columns: 0,
            },
            true,
            &mut out,
        );
        assert_eq!(out, [InputEvent::Mouse(MouseEvent::ScrollDown); 3]);
        out.clear();
        translate(
            Input::Wheel {
                rows: 1000,
                columns: 0,
            },
            true,
            &mut out,
        );
        assert_eq!(out.len(), WHEEL_EVENTS);
        out.clear();
        translate(
            Input::Wheel {
                rows: 0,
                columns: 4,
            },
            true,
            &mut out,
        );
        translate(Input::Click { row: 2, column: 5 }, false, &mut out);
        translate(
            Input::Wheel {
                rows: 3,
                columns: 0,
            },
            false,
            &mut out,
        );
        translate(
            Input::Resize {
                rows: 1,
                columns: 1,
            },
            true,
            &mut out,
        );
        translate(Input::Focus(true), true, &mut out);
        translate(Input::Close, true, &mut out);
        assert!(out.is_empty());
    }
}
