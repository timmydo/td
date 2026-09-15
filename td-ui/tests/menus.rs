#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use td_ui::chrome::{self, Row};
use td_ui::menus::{
    Controller, Error, Event, Fit, Item, Key, Kind, Model, Node, Outcome, Selection,
};
use td_ui::raster::{Composition, Draw, Primitive, Raster, Rect, Scale, Surface, CHROME};

fn row(label: &str, enabled: bool) -> Row<'_> {
    Row {
        label,
        shortcut: "",
        enabled,
        checked: false,
    }
}
fn action(parent: Option<usize>, label: &str, id: u32, enabled: bool) -> Node<'_, u32> {
    Node {
        parent,
        row: row(label, enabled),
        item: Item::Action(id),
    }
}
fn branch(parent: Option<usize>, label: &str) -> Node<'_, u32> {
    Node {
        parent,
        row: row(label, true),
        item: Item::Submenu,
    }
}
fn surface(w: usize, h: usize, scale: u8) -> Surface {
    Surface::new(w, h, Scale::new(scale).unwrap()).unwrap()
}
fn key(menu: &mut Controller<'_, u32, u64>, key: Key) -> Outcome<u32> {
    menu.event(
        Some(9),
        Event::Key {
            key,
            repeated: false,
        },
    )
    .unwrap()
}
fn nodes() -> Vec<Node<'static, u32>> {
    vec![
        branch(None, "Process"),
        action(Some(0), "Disabled", 1, false),
        branch(Some(0), "Send signal"),
        action(Some(2), "Suspend", 2, true),
        branch(Some(2), "More"),
        action(Some(4), "Resume", 3, true),
        action(Some(0), "Terminate", 4, true),
        branch(None, "View"),
        action(Some(7), "Live", 5, true),
    ]
}
fn menu(surface: Surface) -> Controller<'static, u32, u64> {
    let model = Model::new(Kind::Bar, 9, &nodes()).unwrap();
    let mut menu = Controller::new(model, surface, Fit::Adaptive).unwrap();
    menu.open_bar(0).unwrap();
    menu
}

#[test]
fn keyboard_owns_navigation_disabled_rows_submenus_and_one_activation() {
    let mut menu = menu(surface(1000, 600, 1));
    assert_eq!(menu.selection(), Selection::Node(2));
    key(&mut menu, Key::Up);
    assert_eq!(menu.selection(), Selection::Node(6));
    key(&mut menu, Key::Down);
    key(&mut menu, Key::Right);
    assert_eq!(menu.depth(), 2);
    assert_eq!(menu.selection(), Selection::Node(3));
    assert_eq!(
        menu.event(
            Some(9),
            Event::Key {
                key: Key::Activate,
                repeated: true
            }
        )
        .unwrap(),
        Outcome::Consumed
    );
    assert_eq!(menu.depth(), 2);
    key(&mut menu, Key::Down);
    key(&mut menu, Key::Activate);
    assert_eq!(menu.depth(), 3);
    key(&mut menu, Key::Escape);
    assert_eq!(menu.depth(), 2);
    assert_eq!(menu.selection(), Selection::Node(4));
    key(&mut menu, Key::Left);
    assert_eq!(menu.depth(), 1);
    key(&mut menu, Key::Down);
    assert_eq!(key(&mut menu, Key::Activate), Outcome::Activated(4));
    assert!(!menu.is_open());
    assert_eq!(key(&mut menu, Key::Activate), Outcome::Ignored);
}

#[test]
fn header_switches_and_current_header_dismisses_without_click_through() {
    let mut menu = menu(surface(800, 600, 1));
    key(&mut menu, Key::Left);
    assert_eq!(menu.group(), Some(1));
    key(&mut menu, Key::Right);
    assert_eq!(menu.group(), Some(0));
    assert_eq!(
        menu.event(Some(9), Event::Press { x: 8, y: 1 }).unwrap(),
        Outcome::Dismissed
    );
    assert_eq!(
        menu.event(Some(9), Event::Press { x: 8, y: 1 }).unwrap(),
        Outcome::Changed
    );
    assert_eq!(
        menu.event(Some(9), Event::Press { x: 799, y: 599 })
            .unwrap(),
        Outcome::Dismissed
    );
    assert_eq!(
        menu.event(Some(9), Event::Release).unwrap(),
        Outcome::Ignored
    );
    assert!(menu.header_hit(800, 1).is_none());
    assert!(menu.header_hit(-1, 1).is_none());
}

#[test]
fn disabled_headers_and_single_group_navigation_preserve_state() {
    let mut nodes = nodes();
    nodes.get_mut(7).unwrap().row.enabled = false;
    let model = Model::new(Kind::Bar, 9, &nodes).unwrap();
    let mut menu = Controller::new(model, surface(800, 600, 1), Fit::Adaptive).unwrap();
    let header = chrome::Bar::new(menu.surface(), &["Process", "View"])
        .header(1)
        .unwrap();
    assert_eq!(
        menu.event(
            Some(9),
            Event::Press {
                x: header.x,
                y: header.y
            }
        )
        .unwrap(),
        Outcome::Ignored
    );
    menu.open_bar(0).unwrap();
    key(&mut menu, Key::Down);
    assert_eq!(menu.selection(), Selection::Node(6));
    for key_value in [Key::Left, Key::Right] {
        assert_eq!(key(&mut menu, key_value), Outcome::Consumed);
        assert_eq!(menu.selection(), Selection::Node(6));
    }
    assert_eq!(
        menu.event(
            Some(9),
            Event::Press {
                x: header.x,
                y: header.y
            }
        )
        .unwrap(),
        Outcome::Consumed
    );
    assert_eq!(menu.selection(), Selection::Node(6));
    assert_eq!(
        menu.event(Some(9), Event::Other).unwrap(),
        Outcome::Consumed
    );
}

#[test]
fn a_zero_wheel_delta_does_not_close_descendants() {
    let mut menu = menu(surface(1000, 600, 1));
    let parent = menu.row_rect(2).unwrap();
    key(&mut menu, Key::Right);
    assert_eq!(menu.depth(), 2);
    assert_eq!(
        menu.event(
            Some(9),
            Event::Wheel {
                x: parent.x,
                y: parent.y,
                rows: 0
            }
        )
        .unwrap(),
        Outcome::Consumed
    );
    assert_eq!(menu.depth(), 2);
}

#[test]
fn pointer_enters_children_without_closing_the_ancestor_path() {
    for scale in 1..=4 {
        let s = scale as usize;
        let mut menu = menu(surface(1000 * s, 400 * s, scale));
        let branch = menu.row_rect(2).unwrap();
        menu.event(
            Some(9),
            Event::Move {
                x: branch.x + 1,
                y: branch.y + 1,
            },
        )
        .unwrap();
        assert_eq!(menu.depth(), 2);
        let child = menu.row_rect(3).unwrap();
        assert_eq!(child.x, branch.x + i64::from(branch.width));
        menu.event(
            Some(9),
            Event::Move {
                x: child.x,
                y: child.y,
            },
        )
        .unwrap();
        assert_eq!(menu.depth(), 2);
        assert_eq!(
            menu.event(
                Some(9),
                Event::Press {
                    x: child.x,
                    y: child.y
                }
            )
            .unwrap(),
            Outcome::Activated(2)
        );
        assert_eq!(
            menu.event(
                Some(9),
                Event::Press {
                    x: child.x,
                    y: child.y
                }
            )
            .unwrap(),
            Outcome::Ignored
        );
    }
}

#[test]
fn context_children_fit_left_or_replace_with_an_actionable_back_row() {
    let nodes = [branch(None, "Signals"), action(Some(0), "Resume", 7, true)];
    for scale in 1..=4 {
        let s = scale as usize;
        for width in [400, 800] {
            let surface = surface(width * s, 240 * s, scale);
            let model = Model::new(Kind::Context, 9, &nodes).unwrap();
            let mut menu = Controller::new(model, surface, Fit::Adaptive).unwrap();
            menu.open_context(i64::MAX, i64::MAX).unwrap();
            let parent = menu.panel(0).unwrap();
            key(&mut menu, Key::Right);
            let child = menu.panel(1).unwrap();
            assert_eq!(child.intersection(surface.bounds()), Some(child));
            if width == 800 {
                assert!(child.x + i64::from(child.width) <= parent.x);
            } else {
                assert_eq!(child.x, parent.x);
                let mut draws = Vec::new();
                menu.emit(surface.bounds(), &mut |d| draws.push(d));
                let glyphs: String = draws
                    .iter()
                    .filter_map(|d| match d.primitive {
                        Primitive::Glyph { scalar, .. } => Some(scalar),
                        _ => None,
                    })
                    .collect();
                assert!(glyphs.contains("Back"));
                assert!(!glyphs.contains("Signals"));
                key(&mut menu, Key::Up);
                assert_eq!(menu.selection(), Selection::Back);
                key(&mut menu, Key::Activate);
                assert_eq!(menu.depth(), 1);
            }
        }
    }
}

#[test]
fn third_panel_uses_back_instead_of_covering_an_ancestor() {
    for scale in 1..=4 {
        let s = scale as usize;
        let mut menu = menu(surface(648 * s, 400 * s, scale));
        let first = menu.row_rect(2).unwrap();
        menu.event(
            Some(9),
            Event::Move {
                x: first.x,
                y: first.y,
            },
        )
        .unwrap();
        let nested = menu.row_rect(4).unwrap();
        menu.event(
            Some(9),
            Event::Move {
                x: nested.x,
                y: nested.y,
            },
        )
        .unwrap();
        assert_eq!(menu.depth(), 3);
        assert!(menu.row_rect(2).is_none());
        assert!(menu.row_rect(4).is_none());
        assert!(menu.panel(0).is_none());
        assert!(menu.panel(1).is_none());
        let panel = menu.panel(2).unwrap();
        let leaf = menu.row_rect(5).unwrap();
        assert_eq!(leaf.y, panel.y + (24 * s) as i64);
        let mut draws = Vec::new();
        menu.emit(menu.surface().bounds(), &mut |draw| draws.push(draw));
        assert!(draws
            .iter()
            .any(|draw| matches!(draw.primitive, Primitive::Glyph { scalar: 'B', .. })));
        menu.event(
            Some(9),
            Event::Press {
                x: panel.x,
                y: panel.y,
            },
        )
        .unwrap();
        assert_eq!(menu.depth(), 2);
        assert!(menu.row_rect(2).is_some());
        assert!(menu.row_rect(4).is_some());
        key(&mut menu, Key::Right);
        let leaf = menu.row_rect(5).unwrap();
        assert_eq!(
            menu.event(
                Some(9),
                Event::Press {
                    x: leaf.x,
                    y: leaf.y
                }
            )
            .unwrap(),
            Outcome::Activated(3)
        );
    }
}

#[test]
fn scroll_and_keyboard_reveal_share_visible_hit_geometry() {
    let nodes: Vec<_> = (0..30)
        .map(|id| action(None, "Item", id, id != 1))
        .collect();
    let surface = surface(180, 72, 1);
    let mut menu = Controller::new(
        Model::new(Kind::Context, 9, &nodes).unwrap(),
        surface,
        Fit::Adaptive,
    )
    .unwrap();
    menu.open_context(-100, -100).unwrap();
    let panel = menu.panel(0).unwrap();
    assert_eq!(panel.height, 72);
    key(&mut menu, Key::Up);
    assert_eq!(menu.selection(), Selection::Node(29));
    assert!(menu.row_rect(0).is_none());
    let last = menu.row_rect(29).unwrap();
    assert_eq!(last.y, 48);
    menu.event(
        Some(9),
        Event::Wheel {
            x: 1,
            y: 1,
            rows: isize::MIN,
        },
    )
    .unwrap();
    assert!(menu.row_rect(0).is_some());
    assert!(menu.row_rect(29).is_none());
    assert_eq!(key(&mut menu, Key::Activate), Outcome::Consumed);
    key(&mut menu, Key::Down);
    assert_eq!(menu.selection(), Selection::Node(0));
    key(&mut menu, Key::Down);
    assert_eq!(menu.selection(), Selection::Node(2));
    assert_eq!(
        menu.event(Some(9), Event::Press { x: 1, y: 48 }).unwrap(),
        Outcome::Activated(2)
    );
}

#[test]
fn stale_revision_focus_loss_and_resize_cannot_activate_captured_actions() {
    for event in [Event::FocusLost, Event::Resize(surface(900, 600, 1))] {
        let mut menu = menu(surface(800, 600, 1));
        key(&mut menu, Key::Right);
        assert_eq!(menu.event(Some(9), event).unwrap(), Outcome::Dismissed);
        assert_eq!(key(&mut menu, Key::Activate), Outcome::Ignored);
    }
    for revision in [None, Some(10)] {
        let mut menu = menu(surface(800, 600, 1));
        assert_eq!(
            menu.event(revision, Event::Press { x: 8, y: 24 }).unwrap(),
            Outcome::Stale
        );
        assert_eq!(key(&mut menu, Key::Activate), Outcome::Ignored);
    }
    let mut menu = menu(surface(800, 600, 1));
    let bad = Surface {
        width: usize::MAX,
        height: 1,
        scale: Scale::new(1).unwrap(),
    };
    assert_eq!(
        menu.event(Some(9), Event::Resize(bad)).unwrap_err(),
        Error::InvalidSurface
    );
    assert_eq!(menu.surface(), surface(800, 600, 1));
}

#[test]
fn model_bounds_refuse_excess_depth_bad_parents_and_unbounded_text() {
    let many = vec![action(None, "A", 1, true); 257];
    assert_eq!(
        Model::new(Kind::Context, 9, &many).unwrap_err(),
        Error::Limit
    );
    let mut deep: Vec<_> = (0usize..8)
        .map(|i| branch(i.checked_sub(1), "Child"))
        .collect();
    deep.push(action(Some(7), "Too deep", 1, true));
    assert_eq!(
        Model::new(Kind::Context, 9, &deep).unwrap_err(),
        Error::Limit
    );
    deep.remove(8);
    deep[7] = action(Some(6), "Allowed", 1, true);
    let mut menu = Controller::new(
        Model::new(Kind::Context, 9, &deep).unwrap(),
        surface(800, 600, 1),
        Fit::Adaptive,
    )
    .unwrap();
    menu.open_context(0, 0).unwrap();
    for _ in 0..7 {
        key(&mut menu, Key::Right);
    }
    assert_eq!(menu.depth(), 8);
    assert_eq!(key(&mut menu, Key::Activate), Outcome::Activated(1));
    for nodes in [
        vec![branch(Some(0), "Cycle")],
        vec![
            action(None, "Parent", 1, true),
            action(Some(0), "Bad child", 2, true),
        ],
        vec![branch(None, "Empty")],
    ] {
        assert_eq!(
            Model::new(Kind::Context, 9, &nodes).unwrap_err(),
            Error::InvalidModel
        );
    }
    let long = "x".repeat(257);
    assert_eq!(
        Model::new(Kind::Context, 9, &[action(None, &long, 1, true)]).unwrap_err(),
        Error::Limit
    );
    assert_eq!(
        Model::new(Kind::Context, 9, &[action(None, "line\nbreak", 1, true)]).unwrap_err(),
        Error::InvalidModel
    );
}

#[test]
fn complete_roots_consume_wheel_but_their_adaptive_children_scroll() {
    let mut nodes = vec![branch(None, "Actions"), branch(Some(0), "More")];
    nodes.extend((0..30).map(|id| action(Some(1), "Action", id, true)));
    let mut menu = Controller::new(
        Model::new(Kind::Bar, 9, &nodes).unwrap(),
        surface(648, 120, 1),
        Fit::Complete,
    )
    .unwrap();
    menu.open_bar(0).unwrap();
    let root = menu.panel(0).unwrap();
    assert_eq!(
        menu.event(
            Some(9),
            Event::Wheel {
                x: root.x,
                y: root.y,
                rows: 10
            }
        )
        .unwrap(),
        Outcome::Consumed
    );
    key(&mut menu, Key::Activate);
    let child = menu.panel(1).unwrap();
    assert!(menu.row_rect(31).is_none());
    assert_eq!(
        menu.event(
            Some(9),
            Event::Wheel {
                x: child.x,
                y: child.y,
                rows: isize::MAX
            }
        )
        .unwrap(),
        Outcome::Changed
    );
    let last = menu.row_rect(31).unwrap();
    assert_eq!(
        menu.event(
            Some(9),
            Event::Press {
                x: last.x,
                y: last.y
            }
        )
        .unwrap(),
        Outcome::Activated(29)
    );
}

#[test]
fn fully_disabled_complete_panel_keeps_legacy_highlight_without_activation() {
    let nodes = [
        branch(None, "Actions"),
        action(Some(0), "Disabled", 1, false),
    ];
    let surface = surface(400, 120, 1);
    let mut menu = Controller::new(
        Model::new(Kind::Bar, 9, &nodes).unwrap(),
        surface,
        Fit::Complete,
    )
    .unwrap();
    menu.open_bar(0).unwrap();
    let header = chrome::Bar::new(surface, &["Actions"]).header(0).unwrap();
    let panel = chrome::Panel::new(surface, header, 1).unwrap();
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    menu.emit(surface.bounds(), &mut |d| actual.push(d));
    panel.emit(
        nodes.iter().skip(1).map(|n| n.row),
        0,
        surface.bounds(),
        &mut |d| expected.push(d),
    );
    assert_eq!(actual, expected);
    assert_eq!(key(&mut menu, Key::Activate), Outcome::Consumed);
    assert_eq!(key(&mut menu, Key::Down), Outcome::Consumed);
}

#[test]
fn complete_mode_preserves_editor_panel_pixels_and_minima() {
    for scale in 1..=4 {
        let s = scale as usize;
        let nodes = [
            branch(None, "File"),
            action(Some(0), "New", 1, true),
            action(Some(0), "Disabled", 2, false),
        ];
        let model = Model::new(Kind::Bar, 9, &nodes).unwrap();
        let small = surface(320 * s, 95 * s, scale);
        let mut menu = Controller::new(model.clone(), small, Fit::Complete).unwrap();
        assert_eq!(menu.open_bar(0).unwrap_err(), Error::NoRoom);
        let surface = surface(400 * s, 120 * s, scale);
        let mut menu = Controller::new(model, surface, Fit::Complete).unwrap();
        menu.open_bar(0).unwrap();
        let header = chrome::Bar::new(surface, &["File"]).header(0).unwrap();
        let panel = chrome::Panel::new(surface, header, 2).unwrap();
        let mut actual = Vec::new();
        let mut expected = Vec::new();
        menu.emit(surface.bounds(), &mut |d| actual.push(d));
        panel.emit(
            nodes.iter().skip(1).map(|n| n.row),
            0,
            surface.bounds(),
            &mut |d| expected.push(d),
        );
        assert_eq!(actual, expected);
        struct Scene {
            surface: Surface,
            draws: Vec<Draw>,
        }
        impl Composition for Scene {
            fn surface(&self) -> Surface {
                self.surface
            }
            fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
                for draw in &self.draws {
                    if let Some(clip) = draw.clip.intersection(damage) {
                        sink(Draw { clip, ..*draw });
                    }
                }
            }
        }
        let scene = Scene {
            surface,
            draws: actual,
        };
        let face = td_ui::font::pinned().unwrap();
        let mut bytes = vec![0x55; surface.width * surface.height * 4];
        let mut raster = Raster::new(&mut bytes, &face, surface, surface.width * 4).unwrap();
        raster.paint(&scene, surface.bounds()).unwrap();
        let point = (panel.rect().x as usize + 1, panel.rect().y as usize + 1);
        let offset = (point.1 * surface.width + point.0) * 4;
        assert_eq!(
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) & 0xffffff,
            chrome::SELECTED_ROW & 0xffffff
        );
        let offset = ((point.1 + 24 * s) * surface.width + point.0) * 4;
        assert_eq!(
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) & 0xffffff,
            CHROME & 0xffffff
        );
        assert_eq!(&bytes[..4], &[0x55; 4]);
    }
}

#[test]
fn storage_accounting_tracks_nodes_while_labels_remain_borrowed() {
    let short = [action(None, "a", 1, true)];
    let long = [action(None, "a longer borrowed static label", 1, true)];
    let a = Model::new(Kind::Context, 1u64, &short).unwrap();
    let b = Model::new(Kind::Context, 1u64, &long).unwrap();
    assert_eq!(a.storage_bytes(), b.storage_bytes());
    let many = [
        branch(None, "Scope"),
        action(Some(0), "First", 1, true),
        action(Some(0), "Second", 2, true),
    ];
    let c = Model::new(Kind::Context, 1u64, &many).unwrap();
    assert!(c.storage_bytes() > a.storage_bytes());
}
