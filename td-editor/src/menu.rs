//! Bounded menu descriptions and geometry, independent of display and files.

use crate::dialog::Target;
use crate::keys::Profile;
use crate::render::{Draw, Geometry, GlyphStyle, Primitive, Raster, Rect, CHROME, INK};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Group {
    File,
    Edit,
    Format,
    Help,
}

impl Group {
    pub(crate) const ALL: [Self; 4] = [Self::File, Self::Edit, Self::Format, Self::Help];
    pub(crate) fn index(self) -> usize {
        match self {
            Self::File => 0,
            Self::Edit => 1,
            Self::Format => 2,
            Self::Help => 3,
        }
    }
    pub(crate) fn items(self) -> &'static [Item] {
        use Item::*;
        match self {
            Self::File => &[New, Open, Save, SaveAs, Close, Quit],
            Self::Edit => &[Undo, Redo, Cut, Copy, Paste, SelectAll, Windows, Emacs],
            Self::Format => &[Wrap, AutoFill, Fill, Spell],
            Self::Help => &[About],
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
    AutoFill,
    Fill,
    Spell,
    About,
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
            Self::Paste => "Paste",
            Self::SelectAll => "Select All",
            Self::Windows => "Windows key bindings",
            Self::Emacs => "Emacs key bindings",
            Self::Wrap => "Soft Wrap",
            Self::AutoFill => "Auto Fill",
            Self::Fill => "Fill Paragraph",
            Self::Spell => "Spelling (unavailable)",
            Self::About => "About td-editor",
        }
    }
    pub(crate) fn shortcut(self, profile: Profile) -> &'static str {
        match (self, profile) {
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
            (Self::SelectAll, Profile::Windows) => "Ctrl+A",
            (Self::Fill, Profile::Emacs) => "M-q",
            _ => "",
        }
    }
}

pub(crate) struct Menu {
    pub(crate) group: Group,
    pub(crate) selected: usize,
    pub(crate) target: Target,
    pub(crate) profile: Profile,
    pub(crate) file_window: bool,
    pub(crate) undo: bool,
    pub(crate) redo: bool,
    pub(crate) wrap: bool,
    pub(crate) auto_fill: bool,
    pub(crate) copy: bool,
    pub(crate) paste: bool,
}

impl Menu {
    pub(crate) fn enabled(&self, item: Item) -> bool {
        match item {
            Item::Cut | Item::Copy => self.copy,
            Item::Paste => self.paste,
            Item::Spell => false,
            Item::Open | Item::Save | Item::SaveAs => self.file_window,
            Item::Undo => self.undo,
            Item::Redo => self.redo,
            _ => true,
        }
    }
    fn checked(&self, item: Item) -> bool {
        match item {
            Item::Windows => self.profile == Profile::Windows,
            Item::Emacs => self.profile == Profile::Emacs,
            Item::Wrap => self.wrap,
            Item::AutoFill => self.auto_fill,
            _ => false,
        }
    }
    pub(crate) fn panel(&self, geometry: Geometry) -> Option<Rect> {
        let scale = geometry.scale().value();
        let width = 320 * scale;
        let height = self.group.items().len() * 24 * scale;
        let (w, h) = geometry.dimensions();
        if w < width || h < height + 48 * scale {
            return None;
        }
        let header = geometry.menu(self.group.index())?;
        Some(Rect {
            x: header.x.min((w - width) as i64),
            y: (24 * scale) as i64,
            width: width as u32,
            height: height as u32,
        })
    }
    pub(crate) fn hit(&self, geometry: Geometry, x: i64, y: i64) -> Option<usize> {
        let panel = self.panel(geometry)?;
        if !panel.contains(x, y) {
            return None;
        }
        Some((y - panel.y) as usize / (24 * geometry.scale().value()))
    }
    pub(crate) fn step(&mut self, backward: bool) {
        let count = self.group.items().len();
        for _ in 0..count {
            self.selected = if backward {
                (self.selected + count - 1) % count
            } else {
                (self.selected + 1) % count
            };
            if self
                .group
                .items()
                .get(self.selected)
                .is_some_and(|item| self.enabled(*item))
            {
                break;
            }
        }
    }
    pub(crate) fn paint(&self, raster: &mut Raster<'_, '_>, geometry: Geometry) {
        let Some(panel) = self.panel(geometry) else {
            return;
        };
        let scale = geometry.scale().value() as i64;
        for (index, item) in self.group.items().iter().enumerate() {
            let row = Rect {
                y: panel.y + index as i64 * 24 * scale,
                height: (24 * scale) as u32,
                ..panel
            };
            let bg = if index == self.selected {
                0xffc9c1b2
            } else {
                CHROME
            };
            let ink = if self.enabled(*item) { INK } else { 0xff827a6d };
            raster.draw(Draw {
                clip: row,
                primitive: Primitive::Fill {
                    rect: row,
                    color: bg,
                },
            });
            let prefix = if self.checked(*item) { "+ " } else { "  " };
            for (column, scalar) in prefix.chars().chain(item.label().chars()).enumerate() {
                glyph(
                    raster,
                    row,
                    panel.x + (8 + column as i64 * 8) * scale,
                    row.y + 4 * scale,
                    scalar,
                    ink,
                    bg,
                );
            }
            let shortcut = item.shortcut(self.profile);
            let start = panel.x + i64::from(panel.width) - (8 + shortcut.len() as i64 * 8) * scale;
            for (column, scalar) in shortcut.chars().enumerate() {
                glyph(
                    raster,
                    row,
                    start + column as i64 * 8 * scale,
                    row.y + 4 * scale,
                    scalar,
                    ink,
                    bg,
                );
            }
        }
    }
}

fn glyph(raster: &mut Raster<'_, '_>, clip: Rect, x: i64, y: i64, scalar: char, ink: u32, bg: u32) {
    raster.draw(Draw {
        clip,
        primitive: Primitive::Glyph {
            x,
            y,
            scalar,
            style: GlyphStyle::medium(ink, bg),
        },
    });
}

pub(crate) fn header(geometry: Geometry, x: i64, y: i64) -> Option<Group> {
    Group::ALL.into_iter().find(|group| {
        geometry
            .menu(group.index())
            .is_some_and(|rect| rect.contains(x, y))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Scale;

    fn menu(group: Group) -> Menu {
        Menu {
            group,
            selected: 0,
            target: Target {
                tab: 1,
                revision: 0,
            },
            profile: Profile::Windows,
            file_window: true,
            undo: false,
            redo: false,
            wrap: true,
            auto_fill: false,
            copy: false,
            paste: false,
        }
    }

    #[test]
    fn every_complete_panel_and_hit_row_fits_at_scales_one_through_four() {
        for scale in 1..=4 {
            let s = scale as usize;
            for group in Group::ALL {
                let menu = menu(group);
                let height = group.items().len() * 24 + 48;
                let geometry =
                    Geometry::new(320 * s, height * s, Scale::new(scale).unwrap()).unwrap();
                let panel = menu.panel(geometry).unwrap();
                assert_eq!(panel.intersection(geometry.bounds()), Some(panel));
                assert!(panel.y + i64::from(panel.height) <= geometry.status().y);
                for (index, item) in group.items().iter().enumerate() {
                    let y = panel.y + index as i64 * 24 * s as i64;
                    assert_eq!(menu.hit(geometry, panel.x, y), Some(index));
                    assert_eq!(
                        menu.hit(
                            geometry,
                            panel.x + i64::from(panel.width) - 1,
                            y + (24 * s) as i64 - 1
                        ),
                        Some(index)
                    );
                    for profile in [Profile::Windows, Profile::Emacs] {
                        assert!(
                            item.label().chars().count()
                                + item.shortcut(profile).chars().count()
                                + 5
                                <= 40
                        );
                    }
                }
                assert_eq!(menu.hit(geometry, panel.x - 1, panel.y), None);
                assert_eq!(
                    menu.hit(geometry, panel.x, panel.y + i64::from(panel.height)),
                    None
                );
                assert!(menu
                    .panel(
                        Geometry::new(320 * s - 1, height * s, Scale::new(scale).unwrap()).unwrap()
                    )
                    .is_none());
                assert!(menu
                    .panel(
                        Geometry::new(320 * s, height * s - 1, Scale::new(scale).unwrap()).unwrap()
                    )
                    .is_none());
            }
        }
    }

    #[test]
    fn keyboard_navigation_skips_disabled_entries_and_wraps_within_the_group() {
        let mut menu = menu(Group::Edit);
        menu.step(false);
        assert_eq!(
            menu.group.items().get(menu.selected),
            Some(&Item::SelectAll)
        );
        menu.step(true);
        assert_eq!(menu.group.items().get(menu.selected), Some(&Item::Emacs));
        for _ in 0..100 {
            menu.step(false);
            assert!(menu.enabled(*menu.group.items().get(menu.selected).unwrap()));
        }
        assert_eq!(Item::Save.shortcut(Profile::Emacs), "C-x C-s");
        assert_eq!(Item::Save.shortcut(Profile::Windows), "Ctrl+S");
        assert!(menu.checked(Item::Windows));
        assert!(!menu.checked(Item::Emacs));
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
        assert!(geometry.menu(4).is_none());
        assert!(geometry.menu(usize::MAX).is_none());
        let help = geometry.menu(Group::Help.index()).unwrap();
        assert_eq!(help.x + i64::from(help.width), 224);
    }
}
