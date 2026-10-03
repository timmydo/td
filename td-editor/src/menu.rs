//! Bounded menu descriptions and geometry, independent of display and files.

use td_ui::chrome::{self, Bar};
use td_ui::editor_dialog::Target;
use td_ui::editor_keys::Profile;
use td_ui::editor_render::{Geometry, MENU_LABELS};
use td_ui::keys;
use td_ui::menus;
use td_ui::raster::Raster;
#[cfg(test)]
use td_ui::raster::Rect;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Group {
    File,
    Edit,
    Format,
    Help,
    Directory,
}

impl Group {
    pub(crate) const ALL: [Self; 5] = [
        Self::File,
        Self::Edit,
        Self::Format,
        Self::Help,
        Self::Directory,
    ];
    pub(crate) fn index(self) -> usize {
        match self {
            Self::File => 0,
            Self::Edit => 1,
            Self::Format => 2,
            Self::Help => 3,
            Self::Directory => 4,
        }
    }
    pub(crate) fn items(self) -> &'static [Item] {
        use Item::*;
        match self {
            Self::File => &[New, Open, Save, SaveAs, Close, CopyPath, Quit],
            Self::Edit => &[
                Undo,
                Redo,
                Cut,
                Copy,
                Paste,
                SelectAll,
                Windows,
                Emacs,
                Find,
                FindNext,
                FindPrevious,
                Replace,
                GoToLine,
            ],
            Self::Format => &[
                Wrap,
                AutoFill,
                Fill,
                FillColumn,
                Spell,
                Dictionary,
                NextMisspelling,
                PreviousMisspelling,
                LineNumbers,
            ],
            Self::Help => &[About, Command, Keys],
            Self::Directory => &[
                CopyEntryPath,
                SortName,
                SortSize,
                SortModified,
                SortReverse,
                RenameEntry,
                MarkDelete,
                UnmarkDelete,
                DeleteMarked,
                NewDirectory,
                CopyFile,
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Item {
    New,
    Open,
    Save,
    SaveAs,
    Close,
    CopyPath,
    CopyEntryPath,
    RenameEntry,
    MarkDelete,
    UnmarkDelete,
    DeleteMarked,
    NewDirectory,
    CopyFile,
    SortName,
    SortSize,
    SortModified,
    SortReverse,
    Quit,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Windows,
    Emacs,
    Wrap,
    LineNumbers,
    AutoFill,
    Fill,
    FillColumn,
    Spell,
    About,
    Command,
    Keys,
    Find,
    FindNext,
    FindPrevious,
    Replace,
    GoToLine,
    Dictionary,
    NextMisspelling,
    PreviousMisspelling,
}

impl Item {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::New => "New",
            Self::Open => "Open...",
            Self::Save => "Save",
            Self::SaveAs => "Save As...",
            Self::Close => "Close Tab",
            Self::Quit => "Quit",
            Self::Undo => "Undo",
            Self::Redo => "Redo",
            Self::Cut => "Cut",
            Self::Copy => "Copy",
            Self::CopyPath => "Copy Full File Path",
            Self::CopyEntryPath => "Copy Entry Full Path",
            Self::RenameEntry => "Rename / Move",
            Self::MarkDelete => "Mark for Deletion",
            Self::UnmarkDelete => "Unmark Deletion",
            Self::DeleteMarked => "Delete Marked Entries...",
            Self::NewDirectory => "New Directory...",
            Self::CopyFile => "Copy File...",
            Self::SortName => "Sort by Name",
            Self::SortSize => "Sort by Size",
            Self::SortModified => "Sort by Modified",
            Self::SortReverse => "Reverse Sort",
            Self::Paste => "Paste",
            Self::SelectAll => "Select All",
            Self::Windows => "Windows key bindings",
            Self::Emacs => "Emacs key bindings",
            Self::Wrap => "Soft Wrap",
            Self::LineNumbers => "Line Numbers",
            Self::AutoFill => "Auto Fill",
            Self::Fill => "Fill Paragraph",
            Self::FillColumn => "Fill Column...",
            Self::Spell => "Check Spelling",
            Self::Dictionary => "Dictionary...",
            Self::NextMisspelling => "Next Misspelling",
            Self::PreviousMisspelling => "Previous Misspelling",
            Self::About => "About td-editor",
            Self::Command => "Command...",
            Self::Keys => keys::ITEM,
            Self::Find => "Find...",
            Self::FindNext => "Find Next",
            Self::FindPrevious => "Find Previous",
            Self::Replace => "Replace...",
            Self::GoToLine => "Go To Line...",
        }
    }
    /// What the item does, as the key list says it beside the item's
    /// shortcut; the menu panel shows `label`.
    pub(crate) fn what(self) -> &'static str {
        match self {
            Self::New => "open a new untitled tab",
            Self::Open => "open a file",
            Self::Save => "save the document",
            Self::SaveAs => "save the document under a new name",
            Self::Close => "close the tab, asking first about unsaved text",
            Self::Quit => "quit, asking first about unsaved text",
            Self::Undo => "undo the last edit",
            Self::Redo => "redo the last undone edit",
            Self::Cut => "cut the selection to the clipboard",
            Self::Copy => "copy the selection to the clipboard",
            Self::CopyPath => "copy the document's full file path",
            Self::CopyEntryPath => "copy the entry's full path",
            Self::RenameEntry => "rename or move the entry",
            Self::MarkDelete => "mark the entry for deletion",
            Self::UnmarkDelete => "unmark the entry for deletion",
            Self::DeleteMarked => "delete the marked entries, asking first",
            Self::NewDirectory => "make a new directory",
            Self::CopyFile => "copy the file to a new name",
            Self::SortName => "sort the listing by name",
            Self::SortSize => "sort the listing by size",
            Self::SortModified => "sort the listing by modification time",
            Self::SortReverse => "reverse the listing's order",
            Self::Paste => "paste the clipboard's text",
            Self::SelectAll => "select the whole document",
            Self::Windows => "use the Windows key bindings",
            Self::Emacs => "use the Emacs key bindings",
            Self::Wrap => "turn the tab's soft wrap on or off",
            Self::LineNumbers => "show or hide line numbers",
            Self::AutoFill => "turn the tab's Auto Fill on or off",
            Self::Fill => "fill the paragraph to the fill column",
            Self::FillColumn => "set the tab's fill column",
            Self::Spell => "check the document's spelling",
            Self::Dictionary => "load a word list for checking spelling",
            Self::NextMisspelling => "select the next misspelled word",
            Self::PreviousMisspelling => "select the previous misspelled word",
            Self::About => "say what td-editor is",
            Self::Command => "run a named command",
            Self::Keys => "show this list of keys",
            Self::Find => "search the document for text",
            Self::FindNext => "find the next match",
            Self::FindPrevious => "find the previous match",
            Self::Replace => "replace text in the document",
            Self::GoToLine => "go to a line by its number",
        }
    }
    pub(crate) fn shortcut(self, profile: Profile) -> &'static str {
        match (self, profile) {
            (Self::CopyEntryPath, _) => "w",
            (Self::RenameEntry, _) => "R",
            (Self::MarkDelete, _) => "d",
            (Self::UnmarkDelete, _) => "u",
            (Self::DeleteMarked, _) => "x",
            (Self::NewDirectory, _) => "+",
            (Self::CopyFile, _) => "C",
            (Self::SortReverse, _) => "S",
            (Self::Command, Profile::Emacs) => "M-x",
            (Self::Keys, _) => keys::CHORD,
            (Self::New, Profile::Windows) => "Ctrl+N",
            (Self::Open, Profile::Windows) => "Ctrl+O",
            (Self::Open, Profile::Emacs) => "C-x C-f",
            (Self::Save, Profile::Windows) => "Ctrl+S",
            (Self::Save, Profile::Emacs) => "C-x C-s",
            (Self::SaveAs, Profile::Windows) => "Ctrl+Shift+S",
            (Self::SaveAs, Profile::Emacs) => "C-x C-w",
            (Self::Close, Profile::Windows) => "Ctrl+W",
            (Self::Close, Profile::Emacs) => "C-x k",
            (Self::Quit, Profile::Emacs) => "C-x C-c",
            (Self::Undo, Profile::Windows) => "Ctrl+Z",
            (Self::Undo, Profile::Emacs) => "C-/",
            (Self::Redo, Profile::Windows) => "Ctrl+Y",
            (Self::Cut, Profile::Windows) => "Ctrl+X",
            (Self::Cut, Profile::Emacs) => "C-w",
            (Self::Copy, Profile::Windows) => "Ctrl+C",
            (Self::Copy, Profile::Emacs) => "M-w",
            (Self::Paste, Profile::Windows) => "Ctrl+V",
            (Self::Paste, Profile::Emacs) => "C-y",
            (Self::Spell, _) => "F7",
            (Self::GoToLine, _) => "F6",
            (Self::SelectAll, Profile::Windows) => "Ctrl+A",
            (Self::Fill, Profile::Windows) => "Alt+Q",
            (Self::Fill, Profile::Emacs) => "M-q",
            (Self::Find, Profile::Windows) => "Ctrl+F",
            (Self::Replace, Profile::Windows) => "Ctrl+H",
            (Self::Find, Profile::Emacs) => "C-s/C-r",
            (Self::FindNext, Profile::Windows) => "F3",
            (Self::FindPrevious, Profile::Windows) => "Shift+F3",
            _ => "",
        }
    }
}

pub(crate) struct Data {
    pub(crate) target: Target,
    pub(crate) profile: Profile,
    pub(crate) file_window: bool,
    pub(crate) directory: bool,
    pub(crate) directory_entry: bool,
    pub(crate) directory_sort: crate::directory::Sort,
    pub(crate) directory_reverse: bool,
    pub(crate) undo: bool,
    pub(crate) redo: bool,
    pub(crate) wrap: bool,
    pub(crate) line_numbers: bool,
    pub(crate) auto_fill: bool,
    pub(crate) cut: bool,
    pub(crate) copy: bool,
    pub(crate) copy_path: bool,
    pub(crate) paste: bool,
}

impl Data {
    pub(crate) fn enabled(&self, item: Item) -> bool {
        if self.directory
            && matches!(
                item,
                Item::Save
                    | Item::SaveAs
                    | Item::Cut
                    | Item::Paste
                    | Item::Undo
                    | Item::Redo
                    | Item::Wrap
                    | Item::AutoFill
                    | Item::Fill
                    | Item::FillColumn
                    | Item::Spell
                    | Item::Replace
                    | Item::NextMisspelling
                    | Item::PreviousMisspelling
            )
        {
            return false;
        }
        match item {
            Item::CopyEntryPath => self.directory_entry && self.copy_path,
            Item::RenameEntry => self.directory_entry && self.file_window,
            Item::MarkDelete | Item::UnmarkDelete => self.directory_entry && self.file_window,
            Item::DeleteMarked => self.directory && self.file_window,
            Item::NewDirectory => self.directory && self.file_window,
            Item::CopyFile => self.directory_entry && self.file_window,
            Item::SortName | Item::SortSize | Item::SortModified | Item::SortReverse => {
                self.directory
            }
            Item::Cut => self.cut,
            Item::Copy => self.copy,
            Item::CopyPath => self.copy_path,
            Item::Paste => self.paste,
            Item::Dictionary | Item::Open | Item::Save | Item::SaveAs => self.file_window,
            Item::Undo => self.undo,
            Item::Redo => self.redo,
            _ => true,
        }
    }
    fn checked(&self, item: Item) -> bool {
        match item {
            Item::SortName => self.directory && self.directory_sort == crate::directory::Sort::Name,
            Item::SortSize => self.directory && self.directory_sort == crate::directory::Sort::Size,
            Item::SortModified => {
                self.directory && self.directory_sort == crate::directory::Sort::Modified
            }
            Item::SortReverse => self.directory && self.directory_reverse,
            Item::Windows => self.profile == Profile::Windows,
            Item::Emacs => self.profile == Profile::Emacs,
            Item::Wrap => self.wrap,
            Item::LineNumbers => self.line_numbers,
            Item::AutoFill => self.auto_fill,
            _ => false,
        }
    }
    pub(crate) fn stamp(&self) -> Stamp {
        (self.target, self.profile)
    }
}

pub(crate) type Stamp = (Target, Profile);

pub(crate) struct Menu {
    data: Data,
    controller: menus::Controller<'static, Item, Stamp>,
}

impl std::ops::Deref for Menu {
    type Target = Data;
    fn deref(&self) -> &Data {
        &self.data
    }
}

impl Menu {
    pub(crate) fn new(data: Data, group: Group, geometry: Geometry) -> Result<Self, menus::Error> {
        let count = Group::ALL.iter().map(|g| g.items().len() + 1).sum();
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(count)
            .map_err(|_| menus::Error::Allocation)?;
        for group in Group::ALL {
            let parent = nodes.len();
            nodes.push(menus::Node {
                parent: None,
                row: chrome::Row {
                    label: MENU_LABELS
                        .get(group.index())
                        .copied()
                        .ok_or(menus::Error::InvalidModel)?,
                    shortcut: "",
                    enabled: true,
                    checked: false,
                },
                item: menus::Item::Submenu,
            });
            for &item in group.items() {
                nodes.push(menus::Node {
                    parent: Some(parent),
                    row: chrome::Row {
                        label: item.label(),
                        shortcut: item.shortcut(data.profile),
                        enabled: data.enabled(item),
                        checked: data.checked(item),
                    },
                    item: menus::Item::Action(item),
                });
            }
        }
        let model = menus::Model::new(menus::Kind::Bar, data.stamp(), &nodes)?;
        let mut controller =
            menus::Controller::new(model, geometry.surface(), menus::Fit::Complete)?;
        controller.open_bar(group.index())?;
        Ok(Self { data, controller })
    }
    pub(crate) fn valid(&self, stamp: Option<Stamp>, geometry: Geometry) -> bool {
        self.controller.valid(stamp, geometry.surface())
    }
    pub(crate) fn event(
        &mut self,
        stamp: Option<Stamp>,
        event: menus::Event,
    ) -> Result<menus::Outcome<Item>, menus::Error> {
        self.controller.event(stamp, event)
    }
    pub(crate) fn paint(&self, raster: &mut Raster<'_, '_>, geometry: Geometry) {
        if self.controller.surface() == geometry.surface() {
            self.controller
                .emit(geometry.bounds(), &mut |draw| raster.draw(draw));
        }
    }
    #[cfg(test)]
    pub(crate) fn panel(&self, geometry: Geometry) -> Option<Rect> {
        if self.controller.surface() != geometry.surface() {
            return None;
        }
        self.controller.panel(0)
    }
    pub(crate) fn header_group(&self) -> Option<Group> {
        self.controller
            .group()
            .and_then(|index| Group::ALL.get(index))
            .copied()
    }
    #[cfg(test)]
    pub(crate) fn group(&self) -> Group {
        self.header_group().unwrap()
    }
    #[cfg(test)]
    pub(crate) fn selected(&self) -> usize {
        let menus::Selection::Node(index) = self.controller.selection() else {
            panic!("no selection")
        };
        let menus::Item::Action(item) = self.controller.model().node(index).unwrap().item else {
            panic!("not an action")
        };
        self.group()
            .items()
            .iter()
            .position(|i| *i == item)
            .unwrap()
    }
    #[cfg(test)]
    pub(crate) fn row(&self, index: usize) -> Option<Rect> {
        let item = self.group().items().get(index)?;
        (0..menus::ENTRIES).find_map(|node| {
            let entry = self.controller.model().node(node)?;
            match entry.item {
                menus::Item::Action(action) if action == *item => self.controller.row_rect(node),
                _ => None,
            }
        })
    }
    #[cfg(test)]
    pub(crate) fn select(&mut self, index: usize) {
        let row = self.row(index).unwrap();
        self.event(
            Some(self.stamp()),
            menus::Event::Move { x: row.x, y: row.y },
        )
        .unwrap();
    }
    #[cfg(test)]
    fn step(&mut self, backward: bool) {
        self.event(
            Some(self.stamp()),
            menus::Event::Key {
                key: if backward {
                    menus::Key::Up
                } else {
                    menus::Key::Down
                },
                repeated: false,
            },
        )
        .unwrap();
    }
}

pub(crate) fn header(geometry: Geometry, x: i64, y: i64) -> Option<Group> {
    let index = Bar::new(geometry.surface(), &MENU_LABELS).hit(x, y)?;
    Group::ALL.get(index).copied()
}

/// What the window binds outside the menus' items in the Windows profile,
/// spelled as its menus spell their shortcuts.
const WINDOWS_KEYS: &[(&str, &str)] = &[
    ("F10", "open the menus; F10 or Escape closes them"),
    (
        "arrows",
        "move the caret; in a menu, move, and Enter or Space chooses",
    ),
    ("Escape", "cancel a menu, a prompt, a search or a notice"),
    ("Ctrl+Tab", "next tab"),
    ("Ctrl+Shift+Tab", "previous tab"),
    ("Ctrl+Left/Ctrl+Right", "back or forward a word"),
    ("Home/End", "start or end of the line"),
    ("Ctrl+Home/Ctrl+End", "start or end of the document"),
    ("PageUp/PageDown", "up or down a page"),
    (
        "Shift+arrows",
        "extend the selection; Shift extends it with every key above that moves the caret",
    ),
    ("Ctrl+L", "centre the caret's row in the view"),
    ("Ctrl+click", "follow the link under the pointer"),
];

/// What the window binds outside the menus' items in the Emacs profile.
const EMACS_KEYS: &[(&str, &str)] = &[
    ("F10", "open the menus; F10 or C-g closes them"),
    (
        "arrows",
        "move the caret; in a menu, move, and RET or SPC chooses",
    ),
    (
        "C-g/Escape",
        "cancel a prefix, the mark, a menu, a prompt, a search or a notice",
    ),
    ("C-SPC", "set the mark"),
    ("C-a/C-e", "start or end of the line"),
    ("C-b/C-f", "back or forward a character"),
    ("C-p/C-n", "previous or next line"),
    ("M-b/M-f", "back or forward a word"),
    ("Home/End", "start or end of the line"),
    ("C-Home/C-End", "start or end of the document"),
    ("PageUp/PageDown", "up or down a page"),
    ("C-Tab", "next tab"),
    ("C-S-Tab", "previous tab"),
    (
        "S-arrows",
        "extend the selection; so does Shift with Home, End, a page key, C-Home or C-End",
    ),
    ("C-l", "centre the caret's row in the view"),
    ("C-click", "follow the link under the pointer"),
];

/// A directory listing's keys beside its menu's, the same in both
/// profiles but for how each spells Return.
const LISTING_KEYS: &[(&str, &str)] = &[
    ("s", "sort by the next of name, size and modified"),
    ("^", "open the parent directory in place"),
    ("g", "read the directory again"),
    ("q", "close the directory tab"),
];
const WINDOWS_LISTING_OPEN: &[(&str, &str)] = &[
    ("Enter", "open the entry; a directory in place"),
    ("Shift+Enter", "open the entry in a new tab"),
];
const EMACS_LISTING_OPEN: &[(&str, &str)] = &[
    ("RET", "open the entry; a directory in place"),
    ("S-RET", "open the entry in a new tab"),
];

impl Group {
    fn title(self) -> &'static str {
        MENU_LABELS.get(self.index()).copied().unwrap_or_default()
    }

    /// The group's items that `profile` binds a key to, the shortcut as
    /// the menu shows it and what the item does. Help > Keys is left
    /// out: its F1 is the window's own row, which td-ui adds.
    fn rows(self, profile: Profile) -> Vec<keys::Row> {
        self.items()
            .iter()
            .filter(|item| **item != Item::Keys && !item.shortcut(profile).is_empty())
            .map(|item| keys::row((item.shortcut(profile), item.what())))
            .collect()
    }
}

/// The key list's sections for `profile`: a section per menu group,
/// titled as its header, from the shortcuts its items show, then the keys
/// no item carries. The Directory group's section adds a listing's own
/// keys; it is left out of a window that cannot show a listing
/// (`listings` false) and comes first when the active tab is one
/// (`in_listing`). A group with no bound item has no section.
pub(crate) fn key_sections(
    profile: Profile,
    listings: bool,
    in_listing: bool,
) -> Vec<keys::Section> {
    let mut directory = keys::Section {
        title: Group::Directory.title(),
        rows: Group::Directory.rows(profile),
    };
    let open = match profile {
        Profile::Windows => WINDOWS_LISTING_OPEN,
        Profile::Emacs => EMACS_LISTING_OPEN,
    };
    directory
        .rows
        .extend(open.iter().chain(LISTING_KEYS).copied().map(keys::row));
    let mut sections: Vec<keys::Section> = Group::ALL
        .into_iter()
        .filter(|group| *group != Group::Directory)
        .map(|group| keys::Section {
            title: group.title(),
            rows: group.rows(profile),
        })
        .filter(|section| !section.rows.is_empty())
        .collect();
    let other = match profile {
        Profile::Windows => WINDOWS_KEYS,
        Profile::Emacs => EMACS_KEYS,
    };
    if in_listing {
        sections.insert(0, directory);
    } else if listings {
        sections.push(directory);
    }
    sections.push(keys::Section::new("Other keys", other));
    sections
}

#[cfg(test)]
mod tests {
    use super::*;
    use td_ui::raster::Scale;

    fn data() -> Data {
        Data {
            directory: false,
            directory_entry: false,
            directory_sort: crate::directory::Sort::Name,
            directory_reverse: false,
            target: Target {
                tab: 1,
                revision: 0,
            },
            profile: Profile::Windows,
            file_window: true,
            undo: false,
            redo: false,
            wrap: true,
            line_numbers: true,
            auto_fill: false,
            cut: false,
            copy: false,
            copy_path: false,
            paste: false,
        }
    }

    #[test]
    fn complete_panels_keep_their_minima_and_row_geometry_at_all_scales() {
        for scale in 1..=4 {
            let s = scale as usize;
            for group in Group::ALL {
                let height = group.items().len() * 24 + 48;
                let geometry =
                    Geometry::new(320 * s, height * s, Scale::new(scale).unwrap()).unwrap();
                let menu = Menu::new(data(), group, geometry).unwrap();
                let panel = menu.controller.panel(0).unwrap();
                assert_eq!(panel.intersection(geometry.bounds()), Some(panel));
                assert!(panel.y + i64::from(panel.height) <= geometry.status().y);
                for (index, item) in group.items().iter().enumerate() {
                    let rect = menu.row(index).unwrap();
                    assert_eq!(rect.y, panel.y + (index * 24 * s) as i64);
                    assert_eq!(rect.width, panel.width);
                    for profile in [Profile::Windows, Profile::Emacs] {
                        assert!(
                            item.label().chars().count()
                                + item.shortcut(profile).chars().count()
                                + 5
                                <= 40
                        );
                    }
                }
                for (width, height) in [(320 * s - 1, height * s), (320 * s, height * s - 1)] {
                    let geometry =
                        Geometry::new(width, height, Scale::new(scale).unwrap()).unwrap();
                    assert!(matches!(
                        Menu::new(data(), group, geometry),
                        Err(menus::Error::NoRoom)
                    ));
                }
            }
        }
        assert_eq!(Group::File.items().len(), 7);
        assert_eq!(Group::Format.items().len(), 9);
        assert_eq!(Group::Edit.items().len(), 13);
    }

    #[test]
    fn keyboard_navigation_skips_disabled_entries_and_wraps_within_the_group() {
        let mut menu = Menu::new(data(), Group::Edit, Geometry::default()).unwrap();
        assert_eq!(
            menu.group().items().get(menu.selected()),
            Some(&Item::SelectAll)
        );
        menu.step(true);
        assert_eq!(
            menu.group().items().get(menu.selected()),
            Some(&Item::GoToLine)
        );
        for _ in 0..100 {
            menu.step(false);
            assert!(menu.enabled(*menu.group().items().get(menu.selected()).unwrap()));
        }
        assert_eq!(Item::Save.shortcut(Profile::Emacs), "C-x C-s");
        assert_eq!(Item::Save.shortcut(Profile::Windows), "Ctrl+S");
        assert!(menu.checked(Item::Windows));
        assert!(!menu.checked(Item::Emacs));
        assert!(menu.checked(Item::LineNumbers));
    }

    #[test]
    fn cut_can_be_disabled_while_copy_remains_available() {
        let mut available = data();
        available.copy = true;
        assert!(available.enabled(Item::Copy));
        assert!(!available.enabled(Item::Cut));
        available.cut = true;
        assert!(available.enabled(Item::Cut));
    }

    #[test]
    fn painted_header_geometry_is_the_menu_hit_geometry() {
        let geometry = Geometry::default();
        for (index, group) in Group::ALL.into_iter().enumerate() {
            assert_eq!(group.index(), index);
            let rect = geometry.menu(group.index()).unwrap();
            assert_eq!(header(geometry, rect.x, rect.y), Some(group));
            assert_eq!(
                header(geometry, rect.x + i64::from(rect.width) - 1, 23),
                Some(group)
            );
            assert_eq!(header(geometry, rect.x, 24), None);
        }
        assert!(geometry.menu(5).is_none());
        assert!(geometry.menu(usize::MAX).is_none());
        let help = geometry.menu(Group::Help.index()).unwrap();
        assert_eq!(help.x + i64::from(help.width), 248);
    }

    /// The key list's sections are the menus' groups with their bound
    /// items as the menus show them, each profile in its own spelling,
    /// then the keys no item carries; the Directory section is a file
    /// window's, first when a listing is active.
    #[test]
    fn key_sections_are_the_menus_bound_items_then_the_other_keys() {
        let titles = |sections: &[keys::Section]| -> Vec<&str> {
            sections.iter().map(|section| section.title).collect()
        };
        let find = |sections: &[keys::Section], title: &str| -> keys::Section {
            sections
                .iter()
                .find(|section| section.title == title)
                .unwrap()
                .clone()
        };
        let windows = key_sections(Profile::Windows, false, false);
        // Windows binds no Help item but Keys, whose F1 is the window's
        // row, so Help has no section.
        assert_eq!(titles(&windows), ["File", "Edit", "Format", "Other keys"]);
        let emacs = key_sections(Profile::Emacs, true, false);
        assert_eq!(
            titles(&emacs),
            ["File", "Edit", "Format", "Help", "Directory", "Other keys"]
        );
        let listing = key_sections(Profile::Emacs, true, true);
        assert_eq!(
            titles(&listing),
            ["Directory", "File", "Edit", "Format", "Help", "Other keys"]
        );
        for (profile, sections) in [(Profile::Windows, &windows), (Profile::Emacs, &emacs)] {
            for group in [Group::File, Group::Edit, Group::Format] {
                let derived: Vec<keys::Row> = group
                    .items()
                    .iter()
                    .filter(|item| **item != Item::Keys && !item.shortcut(profile).is_empty())
                    .map(|item| keys::Row {
                        keys: item.shortcut(profile),
                        what: item.what(),
                    })
                    .collect();
                assert_eq!(find(sections, group.title()).rows, derived);
            }
        }
        let row = |keys, what| keys::Row { keys, what };
        let file = find(&windows, "File");
        assert!(file.rows.contains(&row("Ctrl+S", "save the document")));
        assert!(!file.rows.iter().any(|r| r.what == Item::Quit.what()));
        let file = find(&emacs, "File");
        assert!(file.rows.contains(&row("C-x C-s", "save the document")));
        assert!(file.rows.contains(&row("C-x C-c", Item::Quit.what())));
        assert_eq!(
            find(&emacs, "Help").rows,
            [row("M-x", "run a named command")]
        );
        assert!(!find(&emacs, "Edit")
            .rows
            .iter()
            .any(|r| r.what == Item::Redo.what()));
        let directory = find(&emacs, "Directory");
        assert_eq!(
            directory.rows.first(),
            Some(&row("w", "copy the entry's full path"))
        );
        for keys in ["S", "R", "d", "u", "x", "+", "C", "RET", "s", "^", "g", "q"] {
            assert!(directory.rows.iter().any(|r| r.keys == keys), "{keys}");
        }
        assert!(
            find(&key_sections(Profile::Windows, true, false), "Directory")
                .rows
                .contains(&row("Shift+Enter", "open the entry in a new tab"))
        );
        let other = find(&windows, "Other keys");
        assert_eq!(other.rows.first().map(|r| r.keys), Some("F10"));
        assert!(other.rows.iter().any(|r| r.keys == "Ctrl+Shift+Tab"));
        let other = find(&emacs, "Other keys");
        assert!(other.rows.iter().any(|r| r.keys == "C-g/Escape"));
        assert!(other.rows.iter().any(|r| r.keys == "C-S-Tab"));
        // The window's own F1 and F12 are the overlay's to add.
        for sections in [&windows, &emacs] {
            assert!(sections
                .iter()
                .flat_map(|section| &section.rows)
                .all(|r| r.keys != keys::CHORD && r.keys != td_ui::theme::CHORD));
        }
    }

    /// Every list the window can show holds to td-ui's style: each
    /// profile's own spelling, alternatives joined by an unspaced `/`,
    /// a description on every row and titles starting with a capital.
    #[test]
    fn key_sections_hold_to_the_key_list_style() {
        for profile in [Profile::Windows, Profile::Emacs] {
            for (listings, in_listing) in [(false, false), (true, false), (true, true)] {
                let problems = keys::check_style(&key_sections(profile, listings, in_listing));
                assert!(
                    problems.is_empty(),
                    "{profile:?} listings={listings} in_listing={in_listing}:\n{}",
                    problems.join("\n")
                );
            }
        }
    }

    /// Help ends with Keys, td-ui's label for the item that opens the
    /// key list, showing F1 in both profiles; every item says what it
    /// does for the list.
    #[test]
    fn help_ends_with_the_key_list_item() {
        assert_eq!(Group::Help.items().last(), Some(&Item::Keys));
        assert_eq!(Group::Help.items().first(), Some(&Item::About));
        assert_eq!(Item::Keys.label(), keys::ITEM);
        assert_eq!(MENU_LABELS.get(Group::Help.index()), Some(&keys::BUTTON));
        for profile in [Profile::Windows, Profile::Emacs] {
            assert_eq!(Item::Keys.shortcut(profile), keys::CHORD);
        }
        for group in Group::ALL {
            for item in group.items() {
                assert!(!item.what().trim().is_empty(), "{item:?}");
                assert_ne!(item.what(), item.label(), "{item:?}");
            }
        }
    }
}
