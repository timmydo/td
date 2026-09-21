//! The client's key vocabulary, and how the window's chords arrive in it.
//!
//! The views were written against a terminal's keys; they keep that
//! vocabulary, and the widget window's chords (`td_ui::window::Input`)
//! are read into it here. A chord with Control, Alt or Shift is not one
//! of the client's keys (a shifted scalar arrives folded, with no shift);
//! those and every other chord the client does not claim are the document
//! pane's when one is shown. A press on a list's row and the wheel's
//! travel over one arrive as keys too, so a view reads the pointer as it
//! reads the keyboard. While a draft is edited every chord is the pane's,
//! and the view's keys are the bar's labels: a request of the pane's
//! kind, which the session serves as it serves the pane's own.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// A press on the list's row at this index of the whole list.
    Click(usize),
    ScrollUp,
    ScrollDown,
    /// A request of the document pane's kind (`save`, `close-tab`), from
    /// a bar label; the session serves it as it serves the pane's own.
    Request(&'static str),
    /// A bar label that opens a dropdown of the view's keys rather than
    /// being one: the session opens it under the label, and the key the
    /// dropdown activates is the view's, as the label would have been.
    Menu(Menu),
}

/// The dropdowns a bar label opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Menu {
    /// The mailbox list's folder actions.
    Folder,
}

/// One row of a dropdown: its label, the shortcut shown at its right,
/// the view's key it is, and whether it needs the view's list to have
/// rows (a folder to delete), without which it is shown disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuRow {
    pub label: &'static str,
    pub shortcut: &'static str,
    pub key: Key,
    pub needs_row: bool,
}

impl Menu {
    /// The dropdown's rows, in order.
    pub fn rows(self) -> &'static [MenuRow] {
        match self {
            Menu::Folder => &[
                MenuRow {
                    label: "New folder",
                    shortcut: "+",
                    key: Key::Char('+'),
                    needs_row: false,
                },
                MenuRow {
                    label: "Delete folder",
                    shortcut: "d",
                    key: Key::Char('d'),
                    needs_row: true,
                },
            ],
        }
    }
}

/// The key a chord names, or none.
pub fn key(chord: &str) -> Option<Key> {
    let mut scalars = chord.chars();
    if let (Some(c), None) = (scalars.next(), scalars.next()) {
        return (!c.is_control()).then_some(Key::Char(c));
    }
    Some(match chord {
        "Space" => Key::Char(' '),
        "Return" => Key::Enter,
        "Escape" => Key::Escape,
        "Backspace" => Key::Backspace,
        "Up" => Key::Up,
        "Down" => Key::Down,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "Home" => Key::Home,
        "End" => Key::End,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_name_the_clients_keys_and_modified_or_unknown_ones_nothing() {
        for (chord, expected) in [
            ("a", Some(Key::Char('a'))),
            ("A", Some(Key::Char('A'))),
            ("+", Some(Key::Char('+'))),
            ("?", Some(Key::Char('?'))),
            ("Space", Some(Key::Char(' '))),
            ("Return", Some(Key::Enter)),
            ("Escape", Some(Key::Escape)),
            ("Backspace", Some(Key::Backspace)),
            ("Up", Some(Key::Up)),
            ("Down", Some(Key::Down)),
            ("PageUp", Some(Key::PageUp)),
            ("PageDown", Some(Key::PageDown)),
            ("Home", Some(Key::Home)),
            ("End", Some(Key::End)),
            ("C-c", None),
            ("C-R", None),
            ("M-Return", None),
            ("S-Up", None),
            ("S-Tab", None),
            ("Tab", None),
            ("Delete", None),
            ("Left", None),
            ("Insert", None),
            ("F1", None),
            ("", None),
        ] {
            assert_eq!(key(chord), expected, "{chord}");
        }
    }
}
