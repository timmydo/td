//! Bounded menu descriptions and geometry, independent of display and files.

use crate::dialog::Target;
use crate::keys::Profile;
use crate::render::{Geometry, MENU_LABELS};
use td_ui::chrome::{self, Bar};
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
            Self::Help => &[About, Command],
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
            Self::Find => "Find...",
            Self::FindNext => "Find Next",
            Self::FindPrevious => "Find Previous",
            Self::Replace => "Replace...",
            Self::GoToLine => "Go To Line...",
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
            (Self::Fill, Profile::Emacs) => "M-q",
            (Self::Find, Profile::Windows) => "Ctrl+F",
            (Self::Replace, Profile::Windows) => "Ctrl+H",
            (Self::Find, Profile::Emacs) => "C-s / C-r",
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
            Item::Cut | Item::Copy => self.copy,
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
}
