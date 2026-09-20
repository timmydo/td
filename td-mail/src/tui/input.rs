//! The client's input vocabulary, and how the window's arrives in it.
//!
//! The views were written against a terminal's keys and SGR mouse
//! reports; they keep that vocabulary, and the screen window's input
//! (`td_ui::screen::Input`) is translated into it here, so a click still
//! carries the terminal's one-based row and column and a wheel frame is
//! one scroll event per row of travel.

use td_ui::screen::{self, Input, Press};

#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
    Ctrl(char),
    AltEnter,
    MouseClick { row: u16, col: u16 },
    ScrollUp,
    ScrollDown,
}

/// The most scroll events one wheel frame becomes, so a fling is bounded.
const WHEEL_EVENTS: usize = 64;

/// The key a press names, or none. A control chord is a `Ctrl` of its
/// letter, as the terminal's control bytes were, except that Control
/// with M or I is the chord here where the terminal's byte was Return
/// or Tab (no view binds a control chord); Alt with Return is
/// `AltEnter`; any other chord with Control, Alt or Shift, and the keys
/// the client never bound (Insert, the function keys), are nothing. The
/// terminal's parser answered those sequences with `Escape`, which the
/// views bind as back, so a shifted arrow left a view; nothing is the
/// answer a chord the client does not name deserves. A shifted scalar
/// arrives folded, with no shift.
pub fn key(press: Press) -> Option<Key> {
    if press.shift && !matches!(press.key, screen::Key::Char(_)) {
        return None;
    }
    if press.control {
        return match press.key {
            screen::Key::Char(c) if c.is_ascii_alphabetic() && !press.alt => {
                Some(Key::Ctrl(c.to_ascii_lowercase()))
            }
            _ => None,
        };
    }
    if press.alt {
        return match press.key {
            screen::Key::Enter => Some(Key::AltEnter),
            _ => None,
        };
    }
    Some(match press.key {
        screen::Key::Char(c) => Key::Char(c),
        screen::Key::Enter => Key::Enter,
        screen::Key::Escape => Key::Escape,
        screen::Key::Backspace => Key::Backspace,
        screen::Key::Tab => Key::Tab,
        screen::Key::Up => Key::Up,
        screen::Key::Down => Key::Down,
        screen::Key::Left => Key::Left,
        screen::Key::Right => Key::Right,
        screen::Key::PageUp => Key::PageUp,
        screen::Key::PageDown => Key::PageDown,
        screen::Key::Home => Key::Home,
        screen::Key::End => Key::End,
        screen::Key::Delete => Key::Delete,
        screen::Key::Insert | screen::Key::Function(_) => return None,
    })
}

fn one_based(cell: usize) -> u16 {
    u16::try_from(cell.saturating_add(1)).unwrap_or(u16::MAX)
}

/// The client's keys for one window input; `mouse` is whether clicks and
/// wheel travel are read at all, off meaning they are nothing. Resize,
/// focus and close carry no key: the window reads those itself.
pub fn translate(input: Input, mouse: bool, out: &mut Vec<Key>) {
    match input {
        Input::Key(press) => {
            if let Some(key) = key(press) {
                out.push(key);
            }
        }
        Input::Click { row, column } if mouse => {
            out.push(Key::MouseClick {
                row: one_based(row),
                col: one_based(column),
            });
        }
        Input::Wheel { rows, .. } if mouse => {
            let event = if rows < 0 {
                Key::ScrollUp
            } else {
                Key::ScrollDown
            };
            let count = rows.unsigned_abs().min(WHEEL_EVENTS);
            out.extend(std::iter::repeat_n(event, count));
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
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn presses_translate_to_the_clients_keys() {
        let mut out = Vec::new();
        for (chord, expected) in [
            ("a", Some(Key::Char('a'))),
            ("A", Some(Key::Char('A'))),
            (" ", Some(Key::Char(' '))),
            ("Space", Some(Key::Char(' '))),
            ("Return", Some(Key::Enter)),
            ("Escape", Some(Key::Escape)),
            ("Backspace", Some(Key::Backspace)),
            ("Tab", Some(Key::Tab)),
            ("Left", Some(Key::Left)),
            ("Right", Some(Key::Right)),
            ("PageDown", Some(Key::PageDown)),
            ("End", Some(Key::End)),
            ("Delete", Some(Key::Delete)),
            ("C-c", Some(Key::Ctrl('c'))),
            ("C-R", Some(Key::Ctrl('r'))),
            ("M-Return", Some(Key::AltEnter)),
            ("C-M-c", None),
            ("C-1", None),
            ("C-Up", None),
            ("M-a", None),
            ("S-Up", None),
            ("S-Tab", None),
            ("S-Return", None),
            ("Insert", None),
            ("F1", None),
        ] {
            out.clear();
            let press = screen::press(chord).expect(chord);
            translate(Input::Key(press), true, &mut out);
            assert_eq!(out, expected.into_iter().collect::<Vec<_>>(), "{chord}");
        }
    }

    #[test]
    fn clicks_carry_the_terminals_cell_and_wheel_travel_is_one_event_per_row() {
        let mut out = Vec::new();
        translate(Input::Click { row: 2, column: 5 }, true, &mut out);
        assert_eq!(out, [Key::MouseClick { row: 3, col: 6 }]);
        out.clear();
        translate(Input::Wheel { rows: -2, columns: 0 }, true, &mut out);
        assert_eq!(out, [Key::ScrollUp, Key::ScrollUp]);
        out.clear();
        translate(Input::Wheel { rows: 3, columns: 1 }, true, &mut out);
        assert_eq!(out, vec![Key::ScrollDown; 3]);
        out.clear();
        translate(Input::Wheel { rows: isize::MAX, columns: 0 }, true, &mut out);
        assert_eq!(out.len(), WHEEL_EVENTS);
        out.clear();
        translate(Input::Wheel { rows: 0, columns: 4 }, true, &mut out);
        assert!(out.is_empty());
        translate(Input::Click { row: 0, column: 0 }, false, &mut out);
        translate(Input::Wheel { rows: 5, columns: 0 }, false, &mut out);
        translate(Input::Resize { rows: 4, columns: 4 }, true, &mut out);
        translate(Input::Focus(true), true, &mut out);
        translate(Input::Close, true, &mut out);
        assert!(out.is_empty());
        translate(Input::Click { row: usize::MAX, column: 70000 }, true, &mut out);
        assert_eq!(out, [Key::MouseClick { row: u16::MAX, col: u16::MAX }]);
    }
}
