#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_ui::raster::{Rect, Scale, Surface};
use td_ui::split::{Axis, Config, Controller, Error, Event, Key, Outcome, Share};
fn setup(axis: Axis, scale: u8) -> (Controller, Surface, Rect) {
    let s = usize::from(scale);
    let surface = Surface::new(700 * s, 400 * s, Scale::new(scale).unwrap()).unwrap();
    let rect = Rect {
        x: 20 * scale as i64,
        y: 30 * scale as i64,
        width: 600 * scale as u32,
        height: 300 * scale as u32,
    };
    (
        Controller::new(
            Config {
                axis,
                first_min: 40,
                second_min: 50,
            },
            Share::default(),
            surface,
            rect,
        )
        .unwrap(),
        surface,
        rect,
    )
}
#[test]
fn geometry_partitions_the_full_rectangle_at_every_scale() {
    for scale in 1..=4 {
        for axis in [Axis::Horizontal, Axis::Vertical] {
            let (mut split, _, rect) = setup(axis, scale);
            for key in [Key::First, Key::Last, Key::Increase, Key::Decrease] {
                split.event(Event::Focus).unwrap();
                split.event(Event::Key(key)).unwrap();
                let layout = split.layout().unwrap();
                let pieces = [layout.first, layout.divider, layout.second];
                let mut area = 0u64;
                for (i, piece) in pieces.iter().enumerate() {
                    assert_eq!(piece.intersection(rect), Some(*piece));
                    area += u64::from(piece.width) * u64::from(piece.height);
                    for other in pieces.iter().skip(i + 1) {
                        assert_eq!(piece.intersection(*other), None);
                    }
                }
                assert_eq!(area, u64::from(rect.width) * u64::from(rect.height));
                let extent = |r: Rect| match axis {
                    Axis::Horizontal => r.width,
                    Axis::Vertical => r.height,
                };
                assert!(extent(layout.first) >= 40 * scale as u32);
                assert!(extent(layout.second) >= 50 * scale as u32);
            }
        }
    }
}
#[test]
fn pointer_capture_clamps_and_focus_or_resize_ends_the_drag() {
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let (mut split, surface, rect) = setup(axis, 1);
        let divider = split.layout().unwrap().divider;
        assert_eq!(
            split.event(Event::Move { x: 0, y: 0 }).unwrap(),
            Outcome::Ignored
        );
        split
            .event(Event::Press {
                x: divider.x + 2,
                y: divider.y + 2,
            })
            .unwrap();
        assert!(split.dragging());
        split
            .event(Event::Move {
                x: i64::MAX,
                y: i64::MAX,
            })
            .unwrap();
        let far = split.layout().unwrap();
        split.event(Event::FocusLost).unwrap();
        assert!(!split.dragging());
        assert!(!split.focused());
        split
            .event(Event::Move {
                x: i64::MIN,
                y: i64::MIN,
            })
            .unwrap();
        assert_eq!(split.layout(), Some(far));
        split
            .event(Event::Press {
                x: far.divider.x,
                y: far.divider.y,
            })
            .unwrap();
        split.event(Event::Resize { surface, rect }).unwrap();
        assert!(!split.dragging());
        split.event(Event::Release { x: 0, y: 0 }).unwrap();
        assert_eq!(split.layout(), Some(far));
    }
}
#[test]
fn narrow_fallback_retires_hits_and_preserves_the_preference() {
    let (mut split, surface, rect) = setup(Axis::Vertical, 1);
    let old = split.layout().unwrap();
    let tiny = Rect { height: 90, ..rect };
    split
        .event(Event::Resize {
            surface,
            rect: tiny,
        })
        .unwrap();
    assert_eq!(split.layout(), None);
    assert_eq!(
        split
            .event(Event::Press {
                x: old.divider.x,
                y: old.divider.y
            })
            .unwrap(),
        Outcome::Ignored
    );
    let mut draws = 0;
    split.emit(surface.bounds(), &mut |_| draws += 1);
    assert_eq!(draws, 0);
    assert_eq!(split.share(), Share::default());
    split.event(Event::Resize { surface, rect }).unwrap();
    assert_eq!(split.layout(), Some(old));
    assert_eq!(
        split.event(Event::Resize {
            surface,
            rect: Rect {
                x: i64::MAX,
                ..rect
            }
        }),
        Err(Error::InvalidRect)
    );
    assert_eq!(split.layout(), None);
    split.event(Event::Resize { surface, rect }).unwrap();
    assert_eq!(split.layout(), Some(old));
}
#[test]
fn only_the_divider_paints_and_accepts_pointer_input() {
    for axis in [Axis::Horizontal, Axis::Vertical] {
        for scale in 1..=4 {
            let (mut split, surface, _) = setup(axis, scale);
            let layout = split.layout().unwrap();
            for piece in [layout.first, layout.second] {
                assert_eq!(
                    split
                        .event(Event::Press {
                            x: piece.x,
                            y: piece.y
                        })
                        .unwrap(),
                    Outcome::Ignored
                );
            }
            let mut count = 0;
            split.emit(surface.bounds(), &mut |draw| {
                count += 1;
                assert_eq!(draw.clip.intersection(layout.divider), Some(draw.clip));
            });
            assert_eq!(count, 2);
            let damage = Rect {
                width: 1,
                height: 1,
                ..layout.divider
            };
            split.emit(damage, &mut |draw| assert_eq!(draw.clip, damage));
        }
        assert_eq!(Share::new(1, 0), None);
        assert_eq!(Share::new(2, 1), None);
    }
}
#[test]
fn temporary_clamping_preserves_share_and_keys_cancel_capture() {
    let (mut split, surface, rect) = setup(Axis::Vertical, 1);
    split.event(Event::Focus).unwrap();
    split.event(Event::Key(Key::Last)).unwrap();
    let original = split.layout();
    let share = split.share();
    split
        .event(Event::Resize {
            surface,
            rect: Rect {
                height: 120,
                ..rect
            },
        })
        .unwrap();
    assert_eq!(split.share(), share);
    assert_eq!(split.layout().unwrap().second.height, 50);
    split.event(Event::Resize { surface, rect }).unwrap();
    assert_eq!(split.layout(), original);
    let divider = split.layout().unwrap().divider;
    split
        .event(Event::Press {
            x: divider.x,
            y: divider.y,
        })
        .unwrap();
    split.event(Event::Key(Key::Decrease)).unwrap();
    let adjusted = split.layout();
    assert!(!split.dragging());
    assert_eq!(
        split.event(Event::Release { x: 0, y: 0 }).unwrap(),
        Outcome::Ignored
    );
    assert_eq!(split.layout(), adjusted);
    assert_eq!(
        Controller::new(
            Config {
                axis: Axis::Vertical,
                first_min: 0,
                second_min: 40
            },
            Share::default(),
            surface,
            rect
        )
        .unwrap_err(),
        Error::InvalidMinimum
    );
    assert_eq!(
        Controller::new(
            Config {
                axis: Axis::Vertical,
                first_min: u32::MAX,
                second_min: 40
            },
            Share::default(),
            surface,
            rect
        )
        .unwrap_err(),
        Error::InvalidMinimum
    );
}
#[test]
fn divider_pixels_and_partial_damage_leave_children_untouched() {
    use td_ui::raster::{Composition, Draw, Raster, SELECTED};
    struct Scene<'a> {
        surface: Surface,
        split: &'a Controller,
    }
    impl Composition for Scene<'_> {
        fn surface(&self) -> Surface {
            self.surface
        }
        fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
            self.split.emit(damage, sink)
        }
    }
    let font = td_ui::font::pinned().unwrap();
    for axis in [Axis::Horizontal, Axis::Vertical] {
        for scale in 1..=4 {
            let (mut split, surface, _) = setup(axis, scale);
            split.event(Event::Focus).unwrap();
            let divider = split.layout().unwrap().divider;
            let scene = Scene {
                surface,
                split: &split,
            };
            let mut whole = vec![0x7a; surface.width * surface.height * 4];
            Raster::new(&mut whole, &font, surface, surface.width * 4)
                .unwrap()
                .paint(&scene, surface.bounds())
                .unwrap();
            let corner = (divider.y as usize * surface.width + divider.x as usize) * 4;
            assert_eq!(
                u32::from_le_bytes(whole[corner..corner + 4].try_into().unwrap()) & 0xffffff,
                SELECTED
            );
            let center = ((divider.y as usize + divider.height as usize / 2) * surface.width
                + divider.x as usize
                + divider.width as usize / 2)
                * 4;
            assert_eq!(
                u32::from_le_bytes(whole[center..center + 4].try_into().unwrap()) & 0xffffff,
                td_ui::raster::CHROME
            );
            let damage = match axis {
                Axis::Horizontal => Rect {
                    x: divider.x + 1,
                    y: divider.y + 11,
                    width: 3,
                    height: 51,
                },
                Axis::Vertical => Rect {
                    x: divider.x + 11,
                    y: divider.y + 1,
                    width: 51,
                    height: 3,
                },
            };
            let mut partial = vec![0x7a; whole.len()];
            Raster::new(&mut partial, &font, surface, surface.width * 4)
                .unwrap()
                .paint(&scene, damage)
                .unwrap();
            for y in 0..surface.height {
                for x in 0..surface.width {
                    let pixel = (y * surface.width + x) * 4;
                    if !divider.contains(x as i64, y as i64) {
                        assert_eq!(&whole[pixel..pixel + 4], &[0x7a; 4]);
                    }
                    if damage.contains(x as i64, y as i64) {
                        assert_eq!(&partial[pixel..pixel + 4], &whole[pixel..pixel + 4]);
                    } else {
                        assert_eq!(&partial[pixel..pixel + 4], &[0x7a; 4]);
                    }
                }
            }
        }
    }
}
#[test]
fn a_stationary_click_keeps_the_exact_preference_even_while_clamped() {
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let (_, surface, wide) = setup(axis, 1);
        let preference = Share::new(1, 7).unwrap();
        let mut split = Controller::new(
            Config {
                axis,
                first_min: 40,
                second_min: 50,
            },
            preference,
            surface,
            wide,
        )
        .unwrap();
        let narrow = match axis {
            Axis::Horizontal => Rect { width: 100, ..wide },
            Axis::Vertical => Rect {
                height: 100,
                ..wide
            },
        };
        for rect in [wide, narrow, wide] {
            split.event(Event::Resize { surface, rect }).unwrap();
            let divider = split.layout().unwrap().divider;
            split
                .event(Event::Press {
                    x: divider.x + 3,
                    y: divider.y + 3,
                })
                .unwrap();
            assert_eq!(
                split
                    .event(Event::Move {
                        x: divider.x + 3,
                        y: divider.y + 3
                    })
                    .unwrap(),
                Outcome::Consumed
            );
            assert_eq!(
                split
                    .event(Event::Release {
                        x: divider.x + 3,
                        y: divider.y + 3
                    })
                    .unwrap(),
                Outcome::Consumed
            );
            assert_eq!(split.share(), preference);
        }
    }
}
#[test]
fn redundant_focus_does_not_repaint_and_a_child_press_retires_old_capture() {
    let (mut split, _, rect) = setup(Axis::Vertical, 1);
    assert_eq!(split.event(Event::Focus).unwrap(), Outcome::Changed);
    assert_eq!(split.event(Event::Focus).unwrap(), Outcome::Consumed);
    let divider = split.layout().unwrap().divider;
    split
        .event(Event::Press {
            x: divider.x + 2,
            y: divider.y + 2,
        })
        .unwrap();
    assert!(split.dragging());
    assert_eq!(
        split
            .event(Event::Press {
                x: rect.x,
                y: rect.y
            })
            .unwrap(),
        Outcome::Ignored
    );
    assert!(!split.dragging());
    let before = split.layout();
    assert_eq!(
        split
            .event(Event::Move {
                x: i64::MAX,
                y: i64::MAX
            })
            .unwrap(),
        Outcome::Ignored
    );
    assert_eq!(
        split
            .event(Event::Release {
                x: i64::MAX,
                y: i64::MAX
            })
            .unwrap(),
        Outcome::Ignored
    );
    assert_eq!(split.layout(), before);
}
#[test]
fn exposed_share_roundtrips_after_drag_across_resize_and_scale() {
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let (mut split, _, _) = setup(axis, 1);
        let divider = split.layout().unwrap().divider;
        split
            .event(Event::Press {
                x: divider.x + 3,
                y: divider.y + 3,
            })
            .unwrap();
        split
            .event(Event::Release {
                x: divider.x + 30,
                y: divider.y + 30,
            })
            .unwrap();
        let (first, total) = split.share().parts();
        let saved = Share::new(first, total).unwrap();
        let (_, surface, rect) = setup(axis, 4);
        split.event(Event::Resize { surface, rect }).unwrap();
        assert_eq!(split.share().parts(), (first, total));
        let restored = Controller::new(
            Config {
                axis,
                first_min: 40,
                second_min: 50,
            },
            saved,
            surface,
            rect,
        )
        .unwrap();
        assert_eq!(restored.layout(), split.layout());
    }
}
#[test]
fn focus_acquired_during_fallback_survives_restored_geometry() {
    let (mut split, surface, rect) = setup(Axis::Vertical, 1);
    assert_eq!(
        split.event(Event::Key(Key::Increase)).unwrap(),
        Outcome::Ignored
    );
    split
        .event(Event::Resize {
            surface,
            rect: Rect { height: 20, ..rect },
        })
        .unwrap();
    assert_eq!(split.event(Event::Focus).unwrap(), Outcome::Changed);
    assert!(split.focused());
    split.event(Event::Resize { surface, rect }).unwrap();
    assert_eq!(
        split.event(Event::Key(Key::Increase)).unwrap(),
        Outcome::Changed
    );
}
#[test]
fn empty_in_bounds_rectangles_fall_back_and_invalid_surfaces_retire_capture() {
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let (mut split, surface, rect) = setup(axis, 1);
        for empty in [
            Rect { width: 0, ..rect },
            Rect { height: 0, ..rect },
            Rect {
                x: surface.width as i64,
                y: surface.height as i64,
                width: 0,
                height: 0,
            },
        ] {
            split
                .event(Event::Resize {
                    surface,
                    rect: empty,
                })
                .unwrap();
            assert_eq!(split.layout(), None);
            let mut count = 0;
            split.emit(surface.bounds(), &mut |_| count += 1);
            assert_eq!(count, 0);
        }
        split.event(Event::Resize { surface, rect }).unwrap();
        let d = split.layout().unwrap().divider;
        split.event(Event::Press { x: d.x, y: d.y }).unwrap();
        assert_eq!(
            split.event(Event::Resize {
                surface: Surface {
                    width: 0,
                    ..surface
                },
                rect
            }),
            Err(Error::InvalidSurface)
        );
        assert!(!split.dragging());
        assert_eq!(split.layout(), None);
        assert_eq!(
            split.event(Event::Move { x: d.x, y: d.y }).unwrap(),
            Outcome::Ignored
        );
    }
}
#[test]
fn returning_a_drag_to_its_start_restores_the_exact_saved_preference() {
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let (_, surface, rect) = setup(axis, 1);
        let preference = Share::new(1, 7).unwrap();
        let small = match axis {
            Axis::Horizontal => Rect { width: 100, ..rect },
            Axis::Vertical => Rect {
                height: 100,
                ..rect
            },
        };
        let mut split = Controller::new(
            Config {
                axis,
                first_min: 40,
                second_min: 50,
            },
            preference,
            surface,
            small,
        )
        .unwrap();
        let d = split.layout().unwrap().divider;
        split
            .event(Event::Press {
                x: d.x + 2,
                y: d.y + 2,
            })
            .unwrap();
        split
            .event(Event::Move {
                x: d.x + 3,
                y: d.y + 3,
            })
            .unwrap();
        split
            .event(Event::Release {
                x: d.x + 2,
                y: d.y + 2,
            })
            .unwrap();
        assert_eq!(split.share(), preference);
    }
}
