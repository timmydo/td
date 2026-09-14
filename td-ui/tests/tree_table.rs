#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_ui::raster::{Rect, Scale, Surface};
use td_ui::tree_table::{Column, Model, Row};
use td_ui::tree_table::{Controller, Event, Focus, Key, Outcome, Target};
fn row(id: u32) -> Row<u32> {
    Row {
        id,
        parent: None,
        depth: 0,
        children: false,
        expanded: false,
    }
}
fn model(rows: &[Row<u32>]) -> Model<u32> {
    Model::new(
        rows,
        &[
            Column {
                title: "Name",
                minimum: 64,
                preferred: 800,
                numeric: false,
            },
            Column {
                title: "CPU",
                minimum: 48,
                preferred: 200,
                numeric: true,
            },
        ],
    )
    .unwrap()
}
fn surface() -> Surface {
    Surface::new(700, 400, Scale::new(1).unwrap()).unwrap()
}
fn rect() -> Rect {
    Rect {
        x: 20,
        y: 20,
        width: 640,
        height: 320,
    }
}
fn controller(rows: &[Row<u32>]) -> Controller<u32> {
    Controller::new(model(rows), surface(), rect()).unwrap()
}
fn key(c: &mut Controller<u32>, key: Key) -> Outcome<u32> {
    c.event(Event::Key {
        key,
        repeated: false,
    })
}
#[test]
fn refresh_preserves_ids_and_scroll_anchor_but_retires_clicks() {
    let rows = (0..100).map(row).collect::<Vec<_>>();
    let mut c = controller(&rows);
    c.select(Some(40), true);
    let anchor = c.first_anchor().unwrap();
    let g = c.geometry().unwrap();
    let point = g.cell(40, 0).unwrap().clip;
    assert_eq!(c.target(point.x + 40, point.y), Some(Target::Row(40)));
    c.event(Event::Press {
        x: point.x + 40,
        y: point.y,
    });
    c.replace(model(&rows)).unwrap();
    assert_eq!(
        c.event(Event::Release {
            x: point.x + 40,
            y: point.y
        }),
        Outcome::Ignored
    );
    c.event(Event::Press {
        x: point.x + 40,
        y: point.y,
    });
    let reversed = rows.iter().rev().copied().collect::<Vec<_>>();
    c.replace(model(&reversed)).unwrap();
    assert_eq!(c.selected(), Some(40));
    assert_eq!(c.first_anchor(), Some(anchor));
    assert_eq!(
        c.event(Event::Release {
            x: point.x + 40,
            y: point.y
        }),
        Outcome::Ignored
    );
    c.replace(model(&[row(200)])).unwrap();
    assert_eq!(c.selected(), None);
    assert_eq!(c.first_anchor(), Some(200));
}
#[test]
fn navigation_selects_reveals_and_emits_parent_child_and_header_intents() {
    let mut root = row(1);
    root.children = true;
    root.expanded = true;
    let child = Row {
        id: 2,
        parent: Some(1),
        depth: 1,
        children: true,
        expanded: false,
    };
    let mut c = controller(&[root, child, row(3)]);
    c.set_focus(Focus::Rows).unwrap();
    assert_eq!(key(&mut c, Key::Down), Outcome::Selected(1));
    assert_eq!(key(&mut c, Key::Right), Outcome::Selected(2));
    assert_eq!(
        key(&mut c, Key::Right),
        Outcome::Disclosure {
            id: 2,
            expanded: true
        }
    );
    assert_eq!(key(&mut c, Key::Left), Outcome::Selected(1));
    assert_eq!(
        key(&mut c, Key::Left),
        Outcome::Disclosure {
            id: 1,
            expanded: false
        }
    );
    assert_eq!(key(&mut c, Key::Last), Outcome::Selected(3));
    assert_eq!(key(&mut c, Key::Activate), Outcome::Activate(3));
    c.set_focus(Focus::Header(0)).unwrap();
    assert_eq!(key(&mut c, Key::Right), Outcome::Consumed);
    assert_eq!(c.focus(), Focus::Header(1));
    assert_eq!(key(&mut c, Key::Activate), Outcome::Sort(1));
    assert!(c.geometry().unwrap().header_cell(1).is_some());
    assert!(c.set_focus(Focus::Header(2)).is_err());
}
#[test]
fn pointer_roles_match_geometry_and_interrupted_actions_never_fire() {
    let root = Row {
        children: true,
        expanded: false,
        ..row(1)
    };
    let mut c = controller(&[root, row(2)]);
    let g = c.geometry().unwrap();
    let d = g.disclosure(0, 0).unwrap();
    assert_eq!(
        c.target(d.x, d.y),
        Some(Target::Disclosure {
            id: 1,
            expanded: true
        })
    );
    for interrupt in [
        Event::Move {
            x: i64::MIN,
            y: i64::MAX,
        },
        Event::FocusLost,
        Event::Other,
        Event::Key {
            key: Key::First,
            repeated: true,
        },
        Event::Scroll {
            rows: 1,
            columns: 0,
        },
    ] {
        c.event(Event::Press { x: d.x, y: d.y });
        c.event(interrupt);
        let expected = if matches!(interrupt, Event::Move { .. }) {
            Outcome::Consumed
        } else {
            Outcome::Ignored
        };
        assert_eq!(c.event(Event::Release { x: d.x, y: d.y }), expected);
    }
    c.event(Event::Press { x: d.x, y: d.y });
    assert_eq!(
        c.event(Event::Release { x: d.x, y: d.y }),
        Outcome::Disclosure {
            id: 1,
            expanded: true
        }
    );
    c.event(Event::Press { x: d.x, y: d.y });
    c.resize(surface(), rect()).unwrap();
    assert_eq!(c.event(Event::Release { x: d.x, y: d.y }), Outcome::Ignored);
    let h = c.geometry().unwrap().header_cell(0).unwrap().clip;
    c.event(Event::Press { x: h.x, y: h.y });
    assert_eq!(c.event(Event::Release { x: h.x, y: h.y }), Outcome::Sort(0));
}
#[test]
fn captured_scrollbars_reach_bounds_and_resize_retires_old_geometry() {
    let rows = (0..100).map(row).collect::<Vec<_>>();
    let mut c = controller(&rows);
    for horizontal in [false, true] {
        let g = c.geometry().unwrap();
        let b = if horizontal {
            g.horizontal().unwrap()
        } else {
            g.vertical()
        };
        c.event(Event::Press {
            x: b.thumb.x + 1,
            y: b.thumb.y + 1,
        });
        c.event(Event::Move {
            x: i64::MAX,
            y: i64::MAX,
        });
        let end = c.geometry().unwrap();
        if horizontal {
            assert_eq!(end.offset(), end.total_width() - end.body().width as usize)
        } else {
            assert_eq!(end.first(), 100 - end.visible())
        }
        c.event(Event::Release {
            x: i64::MIN,
            y: i64::MIN,
        });
        let start = c.geometry().unwrap();
        assert_eq!(
            if horizontal {
                start.offset()
            } else {
                start.first()
            },
            0
        );
    }
    c.resize(
        surface(),
        Rect {
            height: 20,
            ..rect()
        },
    )
    .unwrap();
    assert!(c.geometry().is_none());
    assert_eq!(c.target(30, 30), None);
    assert!(c.resize(surface(), Rect { x: -1, ..rect() }).is_err());
    assert!(c.geometry().is_none());
    c.resize(surface(), rect()).unwrap();
    assert!(c.geometry().is_some());
}
#[test]
fn rendering_visits_only_visible_cells_with_clipped_text_and_disclosure_hits() {
    use td_ui::raster::Primitive;
    use td_ui::tree_table::Cell;
    use td_ui::tree_table::{Direction, Sort};
    let rows = (0..32768).map(row).collect::<Vec<_>>();
    let mut c = controller(&rows);
    c.select(Some(17000), true);
    let g = c.geometry().unwrap();
    let mut visited = Vec::new();
    let mut glyphs = 0;
    c.emit(
        Some(Sort {
            column: 0,
            direction: Direction::Ascending,
        }),
        surface().bounds(),
        &mut |id, col| {
            visited.push((id, col));
            Cell::new("worker").unwrap()
        },
        &mut |draw| {
            assert_eq!(draw.clip.intersection(rect()), Some(draw.clip));
            if let Primitive::Glyph { x, scalar, .. } = draw.primitive {
                if scalar == '^' {
                    glyphs += 1;
                }
                assert!(x >= rect().x - 8);
            }
        },
    );
    assert_eq!(visited.len(), g.visible());
    assert!(visited.iter().all(|(id, col)| *id as usize >= g.first()
        && (*id as usize) < g.first() + g.visible()
        && *col == 0));
    // The first header's sort marker is outside the horizontal viewport.
    assert_eq!(glyphs, 0);
    c.set_width(0, 200).unwrap();
    c.set_width(1, 200).unwrap();
    let g = c.geometry().unwrap();
    let damage = g.cell(g.first(), 1).unwrap().clip;
    visited.clear();
    c.emit(
        None,
        damage,
        &mut |id, col| {
            visited.push((id, col));
            Cell::empty()
        },
        &mut |_| {},
    );
    assert_eq!(visited, vec![(g.first() as u32, 1)]);
}
#[test]
fn tree_pixels_preserve_outside_and_partial_repaint_at_all_scales() {
    use td_ui::raster::{Composition, Draw, Primitive, Raster, PAPER, SELECTED};
    use td_ui::tree_table::Cell;
    use td_ui::tree_table::{Direction, Sort};
    struct Scene(Controller<u32>);
    impl Composition for Scene {
        fn surface(&self) -> Surface {
            self.0.surface()
        }
        fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
            self.0.emit(
                Some(Sort {
                    column: 1,
                    direction: Direction::Descending,
                }),
                damage,
                &mut |_, col| Cell::new(if col == 0 { "worker" } else { "12.50" }).unwrap(),
                sink,
            );
        }
    }
    let font = td_ui::font::pinned().unwrap();
    for s in 1..=4 {
        let surface =
            Surface::new(700 * s as usize, 400 * s as usize, Scale::new(s).unwrap()).unwrap();
        let rect = Rect {
            x: 20 * s as i64,
            y: 20 * s as i64,
            width: 640 * s as u32,
            height: 320 * s as u32,
        };
        let root = Row {
            children: true,
            expanded: true,
            ..row(1)
        };
        let child = Row {
            id: 2,
            parent: Some(1),
            depth: 1,
            ..row(2)
        };
        let mut c = Controller::new(model(&[root, child, row(3)]), surface, rect).unwrap();
        c.set_width(0, 500).unwrap();
        c.set_width(1, 300).unwrap();
        c.select(Some(2), false);
        c.set_focus(Focus::Rows).unwrap();
        c.event(Event::Scroll {
            rows: 0,
            columns: 3,
        });
        let scene = Scene(c);
        let mut contrasted = 0;
        scene.emit(surface.bounds(), &mut |draw| {
            if let Primitive::Glyph { style, .. } = draw.primitive {
                if style.background == SELECTED {
                    assert_eq!(style.ink, PAPER);
                    contrasted += 1;
                }
            }
        });
        assert!(contrasted > 0);
        let mut whole = vec![0x7a; surface.width * surface.height * 4];
        Raster::new(&mut whole, &font, surface, surface.width * 4)
            .unwrap()
            .paint(&scene, surface.bounds())
            .unwrap();
        let damage = Rect {
            x: rect.x + 7,
            y: rect.y + 9,
            width: 480,
            height: 160,
        };
        let mut partial = vec![0x7a; whole.len()];
        Raster::new(&mut partial, &font, surface, surface.width * 4)
            .unwrap()
            .paint(&scene, damage)
            .unwrap();
        for y in 0..surface.height {
            for x in 0..surface.width {
                let p = (y * surface.width + x) * 4;
                if !rect.contains(x as i64, y as i64) {
                    assert_eq!(&whole[p..p + 4], &[0x7a; 4]);
                }
                if damage.contains(x as i64, y as i64) {
                    assert_eq!(&partial[p..p + 4], &whole[p..p + 4]);
                } else {
                    assert_eq!(&partial[p..p + 4], &[0x7a; 4]);
                }
            }
        }
    }
}
#[test]
fn repeated_navigation_reveals_rows_without_repeating_activation() {
    let rows = (0..100).map(row).collect::<Vec<_>>();
    let mut c = controller(&rows);
    c.set_focus(Focus::Rows).unwrap();
    for id in 0..40 {
        assert_eq!(
            c.event(Event::Key {
                key: Key::Down,
                repeated: true
            }),
            Outcome::Selected(id)
        );
        assert!(c.geometry().unwrap().row(id as usize).is_some());
    }
    assert_eq!(
        c.event(Event::Key {
            key: Key::Activate,
            repeated: true
        }),
        Outcome::Consumed
    );
    assert_eq!(
        c.event(Event::Key {
            key: Key::Activate,
            repeated: false
        }),
        Outcome::Activate(39)
    );
    let cell = c.geometry().unwrap().cell(39, 0).unwrap().clip;
    c.event(Event::Press {
        x: cell.x + 40,
        y: cell.y,
    });
    assert!(c.captured());
    c.event(Event::FocusLost);
    assert!(!c.captured());
}

#[test]
fn canceled_pointer_capture_consumes_release_over_other_controls() {
    let mut c = controller(&[row(1), row(2)]);
    let g = c.geometry().unwrap();
    let cell = g.cell(0, 0).unwrap();
    let x = cell.rect.x + 20;
    let y = cell.rect.y + 2;
    for moved in [false, true] {
        assert_eq!(c.event(Event::Press { x, y }), Outcome::Consumed);
        if moved {
            assert_eq!(c.event(Event::Move { x: -20, y: -20 }), Outcome::Consumed);
            assert!(c.captured());
            assert_eq!(c.event(Event::Move { x, y }), Outcome::Consumed);
        }
        assert_eq!(
            c.event(Event::Release { x: -20, y: -20 }),
            Outcome::Consumed
        );
        assert!(!c.captured());
        assert_eq!(c.selected(), None);
        assert_eq!(c.event(Event::Release { x, y }), Outcome::Ignored);
    }
}

#[test]
fn header_and_scrollbar_corners_are_continuous_and_sort_marks_clear_borders() {
    use td_ui::raster::{Draw, Primitive, BORDER, CHROME};
    use td_ui::tree_table::{Cell, Direction, Sort};
    for scale in 1..=4u8 {
        let surface = Surface::new(
            700 * scale as usize,
            400 * scale as usize,
            Scale::new(scale).unwrap(),
        )
        .unwrap();
        let mut c = Controller::new(model(&[row(1)]), surface, surface.bounds()).unwrap();
        for wide in [false, true] {
            c.set_width(0, if wide { 800 } else { 64 }).unwrap();
            let g = c.geometry().unwrap();
            let mut draws: Vec<Draw> = Vec::new();
            c.emit(
                Some(Sort {
                    column: 0,
                    direction: Direction::Ascending,
                }),
                surface.bounds(),
                &mut |_, _| Cell::empty(),
                &mut |draw| draws.push(draw),
            );
            let color = |x: i64, y: i64| {
                draws
                    .iter()
                    .filter_map(|draw| match draw.primitive {
                        Primitive::Fill { rect, color }
                            if draw.clip.contains(x, y) && rect.contains(x, y) =>
                        {
                            Some(color)
                        }
                        _ => None,
                    })
                    .next_back()
            };
            assert_eq!(
                color(g.vertical().track.x + 2, g.header().y + 2),
                Some(CHROME)
            );
            assert_eq!(
                color(
                    g.rect().x + i64::from(g.rect().width) - 2,
                    g.header().y + i64::from(g.header().height) - 1
                ),
                Some(BORDER)
            );
            if let Some(bar) = g.horizontal() {
                assert_eq!(
                    color(g.vertical().track.x + 2, bar.track.y + 2),
                    Some(CHROME)
                );
            }
            for draw in &draws {
                if let Primitive::Glyph { x, scalar: '^', .. } = draw.primitive {
                    let cell = g.header_cell(0).unwrap();
                    assert!(
                        x + (td_ui::CELL_WIDTH * scale as usize) as i64
                            <= cell.rect.x + i64::from(cell.rect.width) - i64::from(scale)
                    );
                }
            }
        }
    }
}

#[test]
fn resizing_preserves_fractional_logical_scroll_offsets() {
    let surface = Surface::new(700, 400, Scale::new(2).unwrap()).unwrap();
    let mut c = Controller::new(model(&[row(1)]), surface, surface.bounds()).unwrap();
    c.set_width(0, 800).unwrap();
    let mut found = false;
    for movement in 1..40 {
        let bar = c.geometry().unwrap().horizontal().unwrap();
        let x = bar.thumb.x + 2;
        let y = bar.thumb.y + 2;
        c.event(Event::Press { x, y });
        c.event(Event::Release { x: x + movement, y });
        let offset = c.geometry().unwrap().offset();
        if offset % 2 == 1 {
            c.resize(surface, surface.bounds()).unwrap();
            assert_eq!(c.geometry().unwrap().offset(), offset);
            let next = Surface::new(700, 400, Scale::new(3).unwrap()).unwrap();
            c.resize(next, next.bounds()).unwrap();
            assert_eq!(c.geometry().unwrap().offset(), offset * 3 / 2);
            found = true;
            break;
        }
    }
    assert!(found, "fixture must reach a fractional logical offset");
}

#[test]
fn focus_on_press_and_redundant_focus_keep_clicks_and_sort_returns_to_rows() {
    let mut c = controller(&[row(1), row(2)]);
    let g = c.geometry().unwrap();
    let cell = g.cell(0, 0).unwrap();
    let point = (cell.rect.x + 4, cell.rect.y + 2);
    assert_eq!(c.target(point.0, point.1), Some(Target::Row(1)));
    c.event(Event::Press {
        x: point.0,
        y: point.1,
    });
    assert_eq!(c.focus(), Focus::Rows);
    c.set_focus(Focus::Rows).unwrap();
    assert!(c.captured());
    assert_eq!(
        c.event(Event::Release {
            x: point.0,
            y: point.1
        }),
        Outcome::Selected(1)
    );
    let header = g.header_cell(0).unwrap().clip;
    c.event(Event::Press {
        x: header.x,
        y: header.y,
    });
    assert_eq!(
        c.event(Event::Release {
            x: header.x,
            y: header.y
        }),
        Outcome::Sort(0)
    );
    assert_eq!(c.focus(), Focus::Rows);
    assert_eq!(key(&mut c, Key::Down), Outcome::Selected(2));
}
#[test]
fn refresh_preserves_widths_and_schema_change_clamps_heading_focus() {
    use td_ui::tree_table::Error;
    let mut c = controller(&[row(1)]);
    c.set_width(0, 500).unwrap();
    c.replace(model(&[row(1), row(2)])).unwrap();
    assert_eq!(c.width(0), Some(500));
    assert_eq!(c.width(2), None);
    assert_eq!(c.set_width(0, 3), Err(Error::InvalidWidth));
    assert_eq!(c.set_width(2, 80), Err(Error::InvalidColumn));
    c.set_focus(Focus::Header(1)).unwrap();
    c.replace(
        Model::new(
            &[row(1)],
            &[Column {
                title: "Other",
                minimum: 80,
                preferred: 100,
                numeric: false,
            }],
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(c.focus(), Focus::Rows);
    assert_eq!(c.width(0), Some(100));
}
#[test]
fn navigation_starts_visible_and_unchanged_selection_is_consumed() {
    let mut c = controller(&(0..100).map(row).collect::<Vec<_>>());
    c.event(Event::Scroll {
        rows: 40,
        columns: 0,
    });
    let first = c.first_anchor().unwrap();
    c.set_focus(Focus::Rows).unwrap();
    assert_eq!(key(&mut c, Key::Down), Outcome::Selected(first));
    assert_eq!(key(&mut c, Key::Left), Outcome::Consumed);
    assert_eq!(key(&mut c, Key::Right), Outcome::Consumed);
    assert_eq!(key(&mut c, Key::First), Outcome::Selected(0));
    assert_eq!(key(&mut c, Key::Up), Outcome::Consumed);
    assert_eq!(key(&mut c, Key::Last), Outcome::Selected(99));
    assert_eq!(key(&mut c, Key::Last), Outcome::Consumed);
}
#[test]
fn numeric_cells_align_right_and_track_press_captures_scroll() {
    use td_ui::raster::Primitive;
    use td_ui::tree_table::Cell;
    let mut c = controller(&(0..100).map(row).collect::<Vec<_>>());
    c.set_width(0, 100).unwrap();
    let g = c.geometry().unwrap();
    let cell = g.cell(0, 1).unwrap();
    let mut first_glyph = None;
    c.emit(
        None,
        cell.clip,
        &mut |_, _| Cell::new("12.50").unwrap(),
        &mut |draw| {
            if let Primitive::Glyph { x, scalar: '1', .. } = draw.primitive {
                first_glyph = Some(x);
            }
        },
    );
    assert_eq!(
        first_glyph,
        Some(cell.rect.x + i64::from(cell.rect.width) - 6 * td_ui::CELL_WIDTH as i64)
    );
    let bar = g.vertical();
    let x = bar.track.x + 2;
    let y = bar.track.y + i64::from(bar.track.height) - 2;
    assert!(!bar.thumb.contains(x, y));
    assert_eq!(c.event(Event::Press { x, y }), Outcome::Consumed);
    assert!(c.captured());
    assert!(c.geometry().unwrap().first() > 0);
    assert_eq!(c.event(Event::Release { x, y }), Outcome::Consumed);
    assert!(!c.captured());
}

#[test]
fn roster_ceiling_leaves_room_for_one_synthetic_root() {
    let mut rows = (0..32769).map(row).collect::<Vec<_>>();
    assert_eq!(model(&rows).rows().len(), 32769);
    rows.push(row(32769));
    assert!(Model::new(
        &rows,
        &[Column {
            title: "Name",
            minimum: 64,
            preferred: 100,
            numeric: false
        }]
    )
    .is_err());
}
