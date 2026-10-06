//! The window's menu bar (DESIGN.md §4): `File`, `Conversation` and
//! `Help` headers on td-ui's bar, their menus td-ui's shared menu
//! controller (td-ui/DESIGN.md, "Shared menu controller") in adaptive
//! fit, as td-mail's Folder menu is. `F10` opens File, as td-editor's
//! does, and a press on a header opens its menu; while one is open the
//! window routes its keys and the pointer to it and paints it after its
//! frame. The controller chooses; the window carries the action out,
//! through the same paths as the item's chord. The Conversation menu
//! shows the open conversation's effort checked, so the window builds the
//! menu again, at a new revision, from the state of the moment it opens.
//! A conversation's row has a context menu of its own (`row`), which the
//! window opens in the same controller at a right press on the row or at
//! `ROW_MENU` for the open conversation's, and puts the bar's back in
//! when it next opens the bar.

use td_ui::chrome::{Bar, Row};
use td_ui::keys;
use td_ui::menus::{self, Controller, Event, Fit, Item, Key, Kind, Model, Node};
use td_ui::raster::Surface;

use crate::config::EFFORTS;

/// The bar's headers: File, Conversation, then Help.
pub const LABELS: &[&str] = &["File", "Conversation", keys::BUTTON];
/// The chord that opens the File menu, and closes it while it is open.
pub const OPEN: &str = "F10";
/// The chord that opens the open conversation's row menu, as a right
/// press on its row does.
pub const ROW_MENU: &str = "S-F10";

/// What a menu item does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Choose a new conversation's workspace template, as `C-n` does.
    New,
    /// Open the Messages window, as `C-S-m` does.
    Messages,
    /// Open the open conversation's workspace card, as `C-S-w` does.
    Workspace,
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
    /// Show td-ui's key list, as `keys::CHORD` does: the window opens it
    /// when the live pointer or keyboard chose this, never the control
    /// socket (`App::input_live`).
    Keys,
    /// Ask whether to delete the open conversation.
    Delete,
    /// Choose the model new conversations start with.
    DefaultModel,
    /// Show archived conversations in the list, or hide them again.
    ShowArchived,
    /// The row menu's: archive its conversation.
    Archive,
    /// The row menu's: bring its archived conversation back.
    Unarchive,
    /// The row menu's: ask whether to delete its conversation.
    DeleteRow,
    /// A background process's row menu: show its output.
    ShowOutput,
    /// A background process's row menu: kill it.
    KillProcess,
    /// Put the open conversation's workspace in `auto` mode, or back in
    /// `ask` (DESIGN.md §11).
    AutoMode,
    /// Compact the open conversation, as `/compact` does (DESIGN.md
    /// §14).
    Compact,
}

/// The File menu's items: label, the chord shown beside it (one that
/// works, or none), and the action.
pub const FILE: &[(&str, &str, Action)] = &[
    ("New conversation\u{2026}", "C-n", Action::New),
    ("Set OpenRouter key\u{2026}", "", Action::SetKey),
    ("Export diagnostics", "", Action::Export),
    ("Messages\u{2026}", "C-S-m", Action::Messages),
    ("Quit", "", Action::Quit),
];
/// The Help menu's items, as `FILE`'s: the key list, shown with td-ui's
/// window chord for it, which the window keeps and the program never
/// sees.
pub const HELP: &[(&str, &str, Action)] = &[(keys::ITEM, keys::CHORD, Action::Keys)];

/// The Conversation menu's item that opens the model picker.
pub const MODEL: &str = "Model\u{2026}";
/// The Conversation menu's item that compacts the open one.
pub const COMPACT: &str = "Compact conversation";
/// The Conversation menu's item that opens the workspace card.
pub const WORKSPACE: &str = "Workspace card\u{2026}";
/// The Conversation menu's submenu of efforts.
pub const EFFORT: &str = "Effort";
/// The Conversation menu's item that chooses the default model.
pub const DEFAULT_MODEL: &str = "Default model\u{2026}";
/// The Conversation menu's item that asks to delete the open one.
pub const DELETE: &str = "Delete conversation\u{2026}";
/// The Conversation menu's item, checked while archived conversations
/// show in the list.
pub const SHOW_ARCHIVED: &str = "Show archived";
/// The Conversation menu's item, checked while the open conversation's
/// workspace is in `auto` mode.
pub const AUTO_MODE: &str = "Auto mode in this workspace";
/// The row menu's items.
pub const ARCHIVE: &str = "Archive";
pub const UNARCHIVE: &str = "Unarchive";
pub const DELETE_ROW: &str = "Delete\u{2026}";
pub const SHOW_OUTPUT: &str = "Show output";
pub const KILL_PROCESS: &str = "Kill";

/// What the Conversation menu shows: whether a conversation is open, its
/// effort, whether its model takes one, and whether archived
/// conversations show.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct State<'a> {
    pub open: bool,
    /// The open conversation has a repository workspace.
    pub workspace: bool,
    pub effort: &'a str,
    pub reasoning: bool,
    pub show_archived: bool,
    /// Whether the open conversation's workspace is in `auto` mode; none
    /// without one.
    pub auto: Option<bool>,
}

pub type Menu = Controller<'static, Action, u64>;

/// The bar over `surface`.
pub fn bar(surface: Surface) -> Bar<'static> {
    Bar::new(surface, LABELS)
}

/// The bar's menus over `surface` at `revision`, closed: a header for
/// each of `LABELS`, in its order, over its items.
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
    nodes.push(Node {
        parent: Some(conversation),
        row: row(WORKSPACE, crate::card::CHORD, state.workspace, false),
        item: Item::Action(Action::Workspace),
    });
    nodes.push(Node {
        parent: Some(conversation),
        row: row(
            AUTO_MODE,
            "",
            state.auto.is_some(),
            state.auto == Some(true),
        ),
        item: Item::Action(Action::AutoMode),
    });
    nodes.push(Node {
        parent: Some(conversation),
        row: row(DEFAULT_MODEL, "", true, false),
        item: Item::Action(Action::DefaultModel),
    });
    nodes.push(Node {
        parent: Some(conversation),
        row: row(COMPACT, "", state.open, false),
        item: Item::Action(Action::Compact),
    });
    nodes.push(Node {
        parent: Some(conversation),
        row: row(DELETE, "", state.open, false),
        item: Item::Action(Action::Delete),
    });
    nodes.push(Node {
        parent: Some(conversation),
        row: row(SHOW_ARCHIVED, "", true, state.show_archived),
        item: Item::Action(Action::ShowArchived),
    });
    let help = nodes.len();
    nodes.push(Node {
        parent: None,
        row: row(keys::BUTTON, "", true, false),
        item: Item::Submenu,
    });
    nodes.extend(HELP.iter().map(|&(label, shortcut, action)| Node {
        parent: Some(help),
        row: row(label, shortcut, true, false),
        item: Item::Action(action),
    }));
    Controller::new(
        Model::new(Kind::Bar, revision, &nodes)?,
        surface,
        Fit::Adaptive,
    )
}

/// A conversation's row menu over `surface` at `revision`, closed:
/// Archive, or Unarchive for an archived conversation, then Delete….
pub fn row(surface: Surface, archived: bool, revision: u64) -> Result<Menu, menus::Error> {
    let item = |label, action| Node {
        parent: None,
        row: Row {
            label,
            shortcut: "",
            enabled: true,
            checked: false,
        },
        item: Item::Action(action),
    };
    let first = if archived {
        item(UNARCHIVE, Action::Unarchive)
    } else {
        item(ARCHIVE, Action::Archive)
    };
    Controller::new(
        Model::new(
            Kind::Context,
            revision,
            &[first, item(DELETE_ROW, Action::DeleteRow)],
        )?,
        surface,
        Fit::Adaptive,
    )
}

/// A background process's row menu over `surface` at `revision`, closed:
/// Show output, then Kill (DESIGN.md §12).
pub fn process(surface: Surface, revision: u64) -> Result<Menu, menus::Error> {
    let item = |label, action| Node {
        parent: None,
        row: Row {
            label,
            shortcut: "",
            enabled: true,
            checked: false,
        },
        item: Item::Action(action),
    };
    Controller::new(
        Model::new(
            Kind::Context,
            revision,
            &[
                item(SHOW_OUTPUT, Action::ShowOutput),
                item(KILL_PROCESS, Action::KillProcess),
            ],
        )?,
        surface,
        Fit::Adaptive,
    )
}

/// A key while the menu is open, as the controller reads it: td-editor's
/// set, `F10` or `S-F10` closing it as either opened it; every other
/// chord is consumed.
pub fn event(chord: &str, repeated: bool) -> Event {
    let key = match chord {
        "Up" => Key::Up,
        "Down" => Key::Down,
        "Left" => Key::Left,
        "Right" => Key::Right,
        "Return" | "Space" | " " => Key::Activate,
        "Escape" => Key::Escape,
        OPEN | ROW_MENU => Key::Dismiss,
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
        workspace: false,
        effort: "high",
        reasoning: true,
        show_archived: false,
        auto: None,
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

    /// Workspace card… is on only for a repository workspace, shows its
    /// chord and activates its action.
    #[test]
    fn the_workspace_card_item_is_on_only_for_a_repository_workspace() {
        let item = |state| {
            nodes(&menu(surface(), state, 1).unwrap())
                .into_iter()
                .find(|node| node.row.label == WORKSPACE)
                .unwrap()
        };
        assert!(!item(OPENED).row.enabled);
        let on = item(State {
            workspace: true,
            ..OPENED
        });
        assert!(on.row.enabled);
        assert_eq!(on.row.shortcut, crate::card::CHORD);
        assert!(matches!(on.item, Item::Action(Action::Workspace)));
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
        // Any conversation open is deleted; nothing is with none open.
        assert!(enabled(OPENED, DELETE) && !enabled(closed, DELETE));
        // The default is chosen with or without one open.
        assert!(enabled(closed, DEFAULT_MODEL) && enabled(OPENED, DEFAULT_MODEL));
        // Show archived is checked while they show.
        let checked = |state: State<'static>| {
            nodes(&menu(surface(), state, 1).unwrap())
                .into_iter()
                .find(|node| node.row.label == SHOW_ARCHIVED)
                .unwrap()
                .row
                .checked
        };
        let shown = State {
            show_archived: true,
            ..closed
        };
        assert!(!checked(OPENED) && checked(shown) && enabled(shown, SHOW_ARCHIVED));
    }

    #[test]
    fn a_rows_menu_archives_or_unarchives_it_and_asks_to_delete_it() {
        for (archived, first) in [(false, Action::Archive), (true, Action::Unarchive)] {
            let mut menu = row(surface(), archived, 1).unwrap();
            assert!(!menu.is_open());
            assert!(menu.open_bar(0).is_err(), "a context menu has no bar");
            menu.open_context(300, 200).unwrap();
            let panel = menu.panel(0).unwrap();
            assert_eq!((panel.x, panel.y), (300, 200));
            assert_eq!(key(&mut menu, "Return"), Outcome::Activated(first));
            menu.open_context(300, 200).unwrap();
            assert_eq!(key(&mut menu, "Down"), Outcome::Changed);
            assert_eq!(
                key(&mut menu, "Return"),
                Outcome::Activated(Action::DeleteRow)
            );
            menu.open_context(300, 200).unwrap();
            assert_eq!(key(&mut menu, "Escape"), Outcome::Dismissed);
        }
        let labels: Vec<&str> = nodes(&row(surface(), true, 1).unwrap())
            .iter()
            .map(|node| node.row.label)
            .collect();
        assert_eq!(labels, [UNARCHIVE, DELETE_ROW]);
        // A background process's: Show output, then Kill.
        let labels: Vec<&str> = nodes(&process(surface(), 1).unwrap())
            .iter()
            .map(|node| node.row.label)
            .collect();
        assert_eq!(labels, [SHOW_OUTPUT, KILL_PROCESS]);
        let mut menu = process(surface(), 1).unwrap();
        menu.open_context(300, 200).unwrap();
        assert_eq!(
            key(&mut menu, "Return"),
            Outcome::Activated(Action::ShowOutput)
        );
        // S-F10 closes it as it opened it.
        let mut menu = row(surface(), false, 1).unwrap();
        menu.open_context(300, 200).unwrap();
        assert_eq!(key(&mut menu, ROW_MENU), Outcome::Dismissed);
        assert!(crate::control::BINDINGS
            .iter()
            .any(|b| b.chord == Some(ROW_MENU)));
    }

    #[test]
    fn its_shortcuts_are_the_window_bindings_chords() {
        for &(label, shortcut, _) in FILE {
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
        // Help's one item shows td-ui's window chord for the key list,
        // which the window keeps: no binding of the program's.
        assert_eq!(HELP, [(keys::ITEM, keys::CHORD, Action::Keys)]);
        assert!(!crate::control::BINDINGS
            .iter()
            .any(|b| b.chord == Some(keys::CHORD)));
    }

    #[test]
    fn help_follows_conversation_and_its_keys_item_is_chosen_by_key_or_press() {
        assert_eq!(LABELS, ["File", "Conversation", keys::BUTTON]);
        // The headers the nodes build are `LABELS`, in its order.
        let headers: Vec<&str> = nodes(&menu(surface(), OPENED, 1).unwrap())
            .into_iter()
            .filter(|node| node.parent.is_none())
            .map(|node| node.row.label)
            .collect();
        assert_eq!(headers, LABELS);
        let mut menu = menu(surface(), OPENED, 1).unwrap();
        menu.open_bar(0).unwrap();
        assert_eq!(key(&mut menu, "Right"), Outcome::Changed);
        assert_eq!(key(&mut menu, "Right"), Outcome::Changed);
        assert_eq!(menu.group(), Some(2));
        assert_eq!(key(&mut menu, "Return"), Outcome::Activated(Action::Keys));
        let header = bar(surface()).header(2).unwrap();
        let press = Event::Press {
            x: header.x + 2,
            y: header.y + 2,
        };
        assert_eq!(menu.event(Some(1), press).unwrap(), Outcome::Changed);
        let row = menu
            .panel(0)
            .map(|panel| (panel.x + 4, panel.y + 4))
            .unwrap();
        let outcome = menu
            .event(Some(1), Event::Press { x: row.0, y: row.1 })
            .unwrap();
        assert_eq!(outcome, Outcome::Activated(Action::Keys));
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
