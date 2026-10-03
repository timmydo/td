//! The window's menu bar (DESIGN.md §4): one `File` header on td-ui's bar,
//! its menu td-ui's shared menu controller (td-ui/DESIGN.md, "Shared menu
//! controller") in adaptive fit, as td-mail's Folder menu is. `F10` opens
//! it, as td-editor's does, and a press on the header does too; while it
//! is open the window routes its keys and the pointer to it and paints it
//! after its frame. The controller chooses; the window carries the
//! action out, through the same paths as the item's chord.

use td_ui::chrome::{Bar, Row};
use td_ui::menus::{self, Controller, Event, Fit, Item, Key, Kind, Model, Node};
use td_ui::raster::Surface;

/// The bar's headers.
pub const LABELS: [&str; 1] = ["File"];
/// The chord that opens the File menu, and closes it while it is open.
pub const OPEN: &str = "F10";
/// The menu's data never changes, so it has one revision.
const REVISION: u64 = 1;

/// What a menu item does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Start a conversation and open it, as `C-n` does.
    New,
    /// Open the dialog that stores the OpenRouter key.
    SetKey,
    /// Close the window, as the compositor's close does.
    Quit,
}

/// The File menu's items: label, the chord shown beside it (one that
/// works, or none), and the action.
pub const FILE: [(&str, &str, Action); 3] = [
    ("New conversation", "C-n", Action::New),
    ("Set OpenRouter key\u{2026}", "", Action::SetKey),
    ("Quit", "", Action::Quit),
];

pub type Menu = Controller<'static, Action, u64>;

/// The bar over `surface`.
pub fn bar(surface: Surface) -> Bar<'static> {
    Bar::new(surface, &LABELS)
}

/// The bar's menus over `surface`, closed.
pub fn menu(surface: Surface) -> Result<Menu, menus::Error> {
    let mut nodes = vec![Node {
        parent: None,
        row: Row {
            label: "File",
            shortcut: "",
            enabled: true,
            checked: false,
        },
        item: Item::Submenu,
    }];
    nodes.extend(FILE.iter().map(|&(label, shortcut, action)| Node {
        parent: Some(0),
        row: Row {
            label,
            shortcut,
            enabled: true,
            checked: false,
        },
        item: Item::Action(action),
    }));
    Controller::new(
        Model::new(Kind::Bar, REVISION, &nodes)?,
        surface,
        Fit::Adaptive,
    )
}

/// The revision every event to the menu carries.
pub fn revision() -> Option<u64> {
    Some(REVISION)
}

/// A key while the menu is open, as the controller reads it: td-editor's
/// set, `F10` closing it as it opened it; every other chord is consumed.
pub fn event(chord: &str, repeated: bool) -> Event {
    let key = match chord {
        "Up" => Key::Up,
        "Down" => Key::Down,
        "Left" => Key::Left,
        "Right" => Key::Right,
        "Return" | "Space" | " " => Key::Activate,
        "Escape" => Key::Escape,
        OPEN => Key::Dismiss,
        _ => return Event::Other,
    };
    Event::Key { key, repeated }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use td_ui::menus::Outcome;
    use td_ui::raster::Scale;

    fn surface() -> Surface {
        Surface::new(1024, 640, Scale::default()).unwrap()
    }

    fn key(menu: &mut Menu, chord: &str) -> Outcome<Action> {
        menu.event(revision(), event(chord, false)).unwrap()
    }

    #[test]
    fn the_file_menu_holds_its_items_in_order_and_each_activates_its_action() {
        for (at, &(_, _, action)) in FILE.iter().enumerate() {
            let mut menu = menu(surface()).unwrap();
            menu.open_bar(0).unwrap();
            for _ in 0..at {
                assert_eq!(key(&mut menu, "Down"), Outcome::Changed);
            }
            assert_eq!(key(&mut menu, "Return"), Outcome::Activated(action));
            assert!(!menu.is_open(), "an action closes the menu");
        }
    }

    #[test]
    fn its_shortcuts_are_the_window_bindings_chords() {
        for (label, shortcut, _) in FILE {
            if !shortcut.is_empty() {
                assert!(
                    crate::control::BINDINGS
                        .iter()
                        .any(|b| b.chord == Some(shortcut)),
                    "{label}: {shortcut}"
                );
            }
        }
        assert!(crate::control::BINDINGS
            .iter()
            .any(|b| b.chord == Some(OPEN)));
    }

    #[test]
    fn f10_and_escape_close_it_and_a_press_on_its_header_opens_it() {
        let mut menu = menu(surface()).unwrap();
        menu.open_bar(0).unwrap();
        assert_eq!(key(&mut menu, OPEN), Outcome::Dismissed);
        menu.open_bar(0).unwrap();
        assert_eq!(key(&mut menu, "Escape"), Outcome::Dismissed);
        // Other chords are the open menu's, consumed.
        menu.open_bar(0).unwrap();
        assert_eq!(key(&mut menu, "C-n"), Outcome::Consumed);
        assert!(menu.is_open());
        menu.dismiss();
        let header = bar(surface()).header(0).unwrap();
        let press = Event::Press {
            x: header.x + 2,
            y: header.y + 2,
        };
        assert_eq!(menu.event(revision(), press).unwrap(), Outcome::Changed);
        assert!(menu.is_open());
        // The pointer chooses: the second item's row.
        let row = menu
            .panel(0)
            .map(|panel| (panel.x + 4, panel.y + 4 + td_ui::chrome::ROW as i64))
            .unwrap();
        let outcome = menu
            .event(revision(), Event::Press { x: row.0, y: row.1 })
            .unwrap();
        assert_eq!(outcome, Outcome::Activated(Action::SetKey));
    }
}
