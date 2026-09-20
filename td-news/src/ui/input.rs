//! The reader's key vocabulary, and how the window's chords arrive in it.
//!
//! The views were written against a terminal's keys; they keep that
//! vocabulary, and the widget window's chords (`td_ui::window::Input`)
//! are read into it here. A chord with Control, Alt or Shift is not one
//! of the reader's keys (a shifted scalar arrives folded, with no
//! shift), and the vocabulary has no Escape, Tab or Delete; those and
//! every other chord the reader does not claim are the document pane's
//! when one is shown.

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

/// The key a chord names, or none.
pub fn key(chord: &str) -> Option<Key> {
    let mut scalars = chord.chars();
    if let (Some(c), None) = (scalars.next(), scalars.next()) {
        return (!c.is_control()).then_some(Key::Char(c));
    }
    Some(match chord {
        "Space" => Key::Char(' '),
        "Return" => Key::Enter,
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
    fn chords_name_the_readers_keys_and_modified_or_unknown_ones_nothing() {
        for (chord, expected) in [
            ("a", Some(Key::Char('a'))),
            ("A", Some(Key::Char('A'))),
            ("/", Some(Key::Char('/'))),
            ("Space", Some(Key::Char(' '))),
            ("Return", Some(Key::Enter)),
            ("Backspace", Some(Key::Backspace)),
            ("Up", Some(Key::Up)),
            ("Down", Some(Key::Down)),
            ("PageUp", Some(Key::PageUp)),
            ("PageDown", Some(Key::PageDown)),
            ("Home", Some(Key::Home)),
            ("End", Some(Key::End)),
            ("C-c", None),
            ("M-Return", None),
            ("S-Up", None),
            ("S-PageDown", None),
            ("Escape", None),
            ("Tab", None),
            ("Delete", None),
            ("F1", None),
            ("", None),
        ] {
            assert_eq!(key(chord), expected, "{chord}");
        }
    }
}
