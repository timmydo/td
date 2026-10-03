//! The window's menu bar (DESIGN.md §4): `File` and `Conversation` headers
//! on td-ui's bar, their menus td-ui's shared menu controller
//! (td-ui/DESIGN.md, "Shared menu controller") in adaptive fit, as
//! td-mail's Folder menu is. `F10` opens it, as td-editor's does, and a
//! press on a header does too; while it is open the window routes its
//! keys and the pointer to it and paints it after its frame. The
//! controller chooses; the window carries the action out, through the
//! same paths as the item's chord. The Conversation menu shows the open
//! conversation's effort checked, so the window builds the menu again,
//! at a new revision, from the state of the moment it opens.

use td_ui::chrome::{Bar, Row};
use td_ui::menus::{self, Controller, Event, Fit, Item, Key, Kind, Model, Node};
use td_ui::raster::Surface;

use crate::config::EFFORTS;

/// The bar's headers.
pub const LABELS: &[&str] = &["File", "Conversation"];
/// The chord that opens the File menu, and closes it while it is open.
pub const OPEN: &str = "F10";

/// What a menu item does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Start a conversation and open it, as `C-n` does.
    New,
    /// Open the dialog that stores the OpenRouter key.
    SetKey,
    /// Write the diagnostics archive (DESIGN.md §4).
    Export,
    /// Close the window, as the compositor's close does.
    Quit,
    /// Open the picker of the open conversation's model.
    Model,
    /// Choose the open conversation's reasoning effort.
    Effort(&'static str),
}

/// The File menu's items: label, the chord shown beside it (one that
/// works, or none), and the action.
pub const FILE: &[(&str, &str, Action)] = &[
    ("New conversation", "C-n", Action::New),
    ("Set OpenRouter key\u{2026}", "", Action::SetKey),
    ("Export diagnostics", "", Action::Export),
    ("Quit", "", Action::Quit),
];

/// The Conversation menu's item that opens the model picker.
pub const MODEL: &str = "Model\u{2026}";
/// The Conversation menu's submenu of efforts.
pub const EFFORT: &str = "Effort";

/// What the Conversation menu shows: whether a conversation is open, its
/// effort, and whether its model takes one.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct State<'a> {
    pub open: bool,
    pub effort: &'a str,
    pub reasoning: bool,
}

pub type Menu = Controller<'static, Action, u64>;

/// The bar over `surface`.
pub fn bar(surface: Surface) -> Bar<'static> {
    Bar::new(surface, LABELS)
}

/// The bar's menus over `surface` at `revision`, closed.
pub fn menu(surface: Surface, state: State<'_>, revision: u64) -> Result<Menu, menus::Error> {
    let row = |label, shortcut, enabled, checked| Row {
        label,
        shortcut,
        enabled,
        checked,
    };
    let mut nodes = vec![Node {
        parent: None,
        row: row("File", "", true, false),
        item: Item::Submenu,
    }];
    nodes.extend(FILE.iter().map(|&(label, shortcut, action)| Node {
        parent: Some(0),
        row: row(label, shortcut, true, false),
        item: Item::Action(action),
    }));
    let conversation = nodes.len();
    nodes.push(Node {
        parent: None,
        row: row("Conversation", "", true, false),
        item: Item::Submenu,
    });
    nodes.push(Node {
        parent: Some(conversation),
        row: row(MODEL, "", state.open, false),
        item: Item::Action(Action::Model),
    });
    let effort = nodes.len();
    nodes.push(Node {
        parent: Some(conversation),
        row: row(EFFORT, "", state.open && state.reasoning, false),
        item: Item::Submenu,
    });
    nodes.extend(EFFORTS.iter().map(|&level| Node {
        parent: Some(effort),
        row: row(level, "", true, level == state.effort),
        item: Item::Action(Action::Effort(level)),
    }));
    Controller::new(
        Model::new(Kind::Bar, revision, &nodes)?,
        surface,
        Fit::Adaptive,
    )
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

    const OPENED: State<'static> = State {
        open: true,
        effort: "high",
        reasoning: true,
    };

    fn surface() -> Surface {
        Surface::new(1024, 640, Scale::default()).unwrap()
    }

    fn key(menu: &mut Menu, chord: &str) -> Outcome<Action> {
        menu.event(Some(1), event(chord, false)).unwrap()
    }

    fn nodes(menu: &Menu) -> Vec<Node<'static, Action>> {
        (0..).map_while(|n| menu.model().node(n).copied()).collect()
    }

    #[test]
    fn the_file_menu_holds_its_items_in_order_and_each_activates_its_action() {
        for (at, &(_, _, action)) in FILE.iter().enumerate() {
            let mut menu = menu(surface(), OPENED, 1).unwrap();
            menu.open_bar(0).unwrap();
            for _ in 0..at {
                assert_eq!(key(&mut menu, "Down"), Outcome::Changed);
            }
            assert_eq!(key(&mut menu, "Return"), Outcome::Activated(action));
            assert!(!menu.is_open(), "an action closes the menu");
        }
    }

    #[test]
    fn the_conversation_menu_opens_the_picker_and_chooses_an_effort() {
        let mut first = menu(surface(), OPENED, 1).unwrap();
        first.open_bar(1).unwrap();
        assert_eq!(key(&mut first, "Return"), Outcome::Activated(Action::Model));
        // Effort is a submenu of every level, the conversation's checked.
        for (at, &level) in EFFORTS.iter().enumerate() {
            let mut menu = menu(surface(), OPENED, 1).unwrap();
            menu.open_bar(1).unwrap();
            assert_eq!(key(&mut menu, "Down"), Outcome::Changed);
            assert_eq!(key(&mut menu, "Right"), Outcome::Changed);
            let node = nodes(&menu)
                .into_iter()
                .find(|node| matches!(node.item, Item::Action(Action::Effort(l)) if l == level))
                .unwrap();
            assert_eq!(node.row.checked, level == "high", "{level}");
            for _ in 0..at {
                assert_eq!(key(&mut menu, "Down"), Outcome::Changed);
            }
            assert_eq!(
                key(&mut menu, "Return"),
                Outcome::Activated(Action::Effort(level))
            );
        }
    }

    #[test]
    fn without_a_conversation_or_a_reasoning_model_its_items_are_off() {
        let enabled = |state: State<'static>, label: &str| {
            nodes(&menu(surface(), state, 1).unwrap())
                .into_iter()
                .find(|node| node.row.label == label)
                .unwrap()
                .row
                .enabled
        };
        assert!(enabled(OPENED, MODEL) && enabled(OPENED, EFFORT));
        let closed = State {
            open: false,
            ..OPENED
        };
        assert!(!enabled(closed, MODEL) && !enabled(closed, EFFORT));
        let plain = State {
            reasoning: false,
            ..OPENED
        };
        assert!(enabled(plain, MODEL) && !enabled(plain, EFFORT));
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
        let mut menu = menu(surface(), OPENED, 1).unwrap();
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
        assert_eq!(menu.event(Some(1), press).unwrap(), Outcome::Changed);
        assert!(menu.is_open());
        // The pointer chooses: the second item's row.
        let row = menu
            .panel(0)
            .map(|panel| (panel.x + 4, panel.y + 4 + td_ui::chrome::ROW as i64))
            .unwrap();
        let outcome = menu
            .event(Some(1), Event::Press { x: row.0, y: row.1 })
            .unwrap();
        assert_eq!(outcome, Outcome::Activated(Action::SetKey));
        // The second header opens the Conversation menu.
        let header = bar(surface()).header(1).unwrap();
        let press = Event::Press {
            x: header.x + 2,
            y: header.y + 2,
        };
        assert_eq!(menu.event(Some(1), press).unwrap(), Outcome::Changed);
        assert_eq!(menu.group(), Some(1));
    }
}
