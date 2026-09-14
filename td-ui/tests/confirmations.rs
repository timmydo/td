#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use td_ui::confirmations::{Choice, Controller, Error, Event, Focus, Key, Model, Outcome};
use td_ui::raster::{Rect, Scale, Surface};

fn surface(scale: u8) -> Surface {
    let s = scale as usize;
    Surface::new(700 * s, 400 * s, Scale::new(scale).unwrap()).unwrap()
}
fn rect(scale: u8) -> Rect {
    let s = u32::from(scale);
    Rect {
        x: 16 * i64::from(scale),
        y: 24 * i64::from(scale),
        width: 600 * s,
        height: 300 * s,
    }
}
fn dialog(scale: u8) -> Controller<u32, u64, u32> {
    Controller::new(
        Model::new(
            "Suspend processes?",
            "Send SIGSTOP",
            &["PID 7 worker", "PID 8 child"],
            19,
            9,
        )
        .unwrap(),
        surface(scale),
        rect(scale),
        Some(42),
    )
    .unwrap()
}
fn event(d: &mut Controller<u32, u64, u32>, event: Event) -> Outcome<u32, u32> {
    d.event(Some(9), true, event)
}
fn key(d: &mut Controller<u32, u64, u32>, key: Key) -> Outcome<u32, u32> {
    event(
        d,
        Event::Key {
            key,
            repeated: false,
        },
    )
}
fn press(rect: Rect) -> Event {
    Event::Press {
        x: rect.x,
        y: rect.y,
    }
}
fn release(rect: Rect) -> Event {
    Event::Release {
        x: rect.x,
        y: rect.y,
    }
}

#[test]
fn cancel_is_default_and_focus_cycles_only_within_the_dialog() {
    let mut d = dialog(1);
    assert_eq!(d.prior_focus(), Some(42));
    assert_eq!(d.focus(), Focus::Cancel);
    assert_eq!(
        key(&mut d, Key::Activate),
        Outcome::Closed {
            choice: Choice::Cancelled,
            restore_focus: Some(42)
        }
    );
    assert_eq!(key(&mut d, Key::Activate), Outcome::Ignored);
    let mut d = dialog(1);
    for _ in 0..20 {
        key(&mut d, Key::Tab);
        assert_eq!(d.focus(), Focus::Confirm);
        key(&mut d, Key::Tab);
        assert_eq!(d.focus(), Focus::Details);
        key(&mut d, Key::Tab);
        assert_eq!(d.focus(), Focus::Cancel);
    }
    key(&mut d, Key::BackTab);
    assert_eq!(d.focus(), Focus::Details);
    assert_eq!(key(&mut d, Key::Activate), Outcome::Consumed);
    key(&mut d, Key::BackTab);
    assert_eq!(d.focus(), Focus::Confirm);
    key(&mut d, Key::BackTab);
    assert_eq!(d.focus(), Focus::Cancel);
    assert_eq!(
        key(&mut d, Key::Escape),
        Outcome::Closed {
            choice: Choice::Cancelled,
            restore_focus: Some(42)
        }
    );
}

#[test]
fn confirmation_requires_a_fresh_choice_and_happens_once() {
    for scale in 1..=4 {
        let mut d = dialog(scale);
        let action = d.action_rect(Focus::Confirm).unwrap();
        assert_eq!(event(&mut d, release(action)), Outcome::Consumed);
        assert_eq!(event(&mut d, press(action)), Outcome::Changed);
        assert_eq!(
            event(
                &mut d,
                Event::Key {
                    key: Key::Activate,
                    repeated: true
                }
            ),
            Outcome::Consumed
        );
        assert!(d.is_open());
        assert_eq!(event(&mut d, release(action)), Outcome::Consumed);
        assert_eq!(event(&mut d, press(action)), Outcome::Changed);
        assert_eq!(
            event(&mut d, release(action)),
            Outcome::Closed {
                choice: Choice::Confirmed(19),
                restore_focus: Some(42)
            }
        );
        assert_eq!(event(&mut d, press(action)), Outcome::Ignored);
        assert_eq!(event(&mut d, release(action)), Outcome::Ignored);
    }
    let mut d = dialog(1);
    key(&mut d, Key::Tab);
    assert_eq!(
        key(&mut d, Key::Activate),
        Outcome::Closed {
            choice: Choice::Confirmed(19),
            restore_focus: Some(42)
        }
    );
}

#[test]
fn outside_input_and_interrupted_pointer_gestures_never_confirm() {
    let mut d = dialog(1);
    assert_eq!(event(&mut d, Event::Other), Outcome::Consumed);
    let confirm = d.action_rect(Focus::Confirm).unwrap();
    let cancel = d.action_rect(Focus::Cancel).unwrap();
    for coordinate in [i64::MIN, -1, i64::MAX] {
        assert_eq!(
            event(
                &mut d,
                Event::Press {
                    x: coordinate,
                    y: coordinate
                }
            ),
            Outcome::Consumed
        );
        assert!(d.is_open());
    }
    event(&mut d, press(confirm));
    assert_eq!(event(&mut d, release(cancel)), Outcome::Consumed);
    event(&mut d, press(confirm));
    event(&mut d, Event::Move { x: 0, y: 0 });
    event(
        &mut d,
        Event::Move {
            x: confirm.x,
            y: confirm.y,
        },
    );
    assert_eq!(event(&mut d, release(confirm)), Outcome::Consumed);
    event(&mut d, press(confirm));
    assert_eq!(
        event(&mut d, Event::FocusLost),
        Outcome::Closed {
            choice: Choice::Cancelled,
            restore_focus: Some(42)
        }
    );
    assert_eq!(event(&mut d, release(confirm)), Outcome::Ignored);
}

#[test]
fn stale_requests_and_missing_previous_controls_cannot_be_reused() {
    for revision in [None, Some(10)] {
        let mut d = dialog(1);
        let confirm = d.action_rect(Focus::Confirm).unwrap();
        event(&mut d, press(confirm));
        assert_eq!(
            d.event(revision, false, release(confirm)),
            Outcome::Closed {
                choice: Choice::Stale,
                restore_focus: None
            }
        );
        assert_eq!(event(&mut d, release(confirm)), Outcome::Ignored);
    }
    let mut d = dialog(1);
    assert_eq!(
        d.event(
            Some(9),
            false,
            Event::Key {
                key: Key::Escape,
                repeated: false
            }
        ),
        Outcome::Closed {
            choice: Choice::Cancelled,
            restore_focus: None
        }
    );
}

#[test]
fn resizing_disarms_and_layout_failure_closes_the_request() {
    let mut d = dialog(1);
    let confirm = d.action_rect(Focus::Confirm).unwrap();
    event(&mut d, press(confirm));
    assert_eq!(
        event(
            &mut d,
            Event::Resize {
                surface: surface(2),
                rect: rect(2)
            }
        ),
        Outcome::Changed
    );
    assert_eq!(d.focus(), Focus::Cancel);
    let confirm = d.action_rect(Focus::Confirm).unwrap();
    assert_eq!(event(&mut d, release(confirm)), Outcome::Consumed);
    event(&mut d, press(confirm));
    let tiny = Rect {
        width: 1,
        height: 1,
        ..rect(2)
    };
    assert_eq!(
        event(
            &mut d,
            Event::Resize {
                surface: surface(2),
                rect: tiny
            }
        ),
        Outcome::Closed {
            choice: Choice::Unavailable(Error::NoRoom),
            restore_focus: Some(42)
        }
    );
    assert_eq!(event(&mut d, release(confirm)), Outcome::Ignored);
}

#[test]
fn complete_details_wrap_without_loss_and_scroll_independently_of_actions() {
    let text = "Pid 123 worker: /long/path/λ世界 ".repeat(100);
    for scale in 1..=4 {
        let model = Model::new("Suspend?", "Send SIGSTOP", &[text.as_str()], 19, 9).unwrap();
        let mut d = Controller::new(model, surface(scale), rect(scale), Some(42)).unwrap();
        let reconstructed: String = (0..d.detail_rows())
            .map(|row| d.detail_text(row).unwrap())
            .collect();
        assert_eq!(reconstructed, text);
        let confirm = d.action_rect(Focus::Confirm).unwrap();
        key(&mut d, Key::BackTab);
        key(&mut d, Key::End);
        assert!(d.first() > 0);
        assert_eq!(d.action_rect(Focus::Confirm), Some(confirm));
        let details = d.details_rect();
        event(
            &mut d,
            Event::Wheel {
                x: details.x,
                y: details.y,
                rows: isize::MIN,
            },
        );
        assert_eq!(d.first(), 0);
        key(&mut d, Key::PageDown);
        assert!(d.first() > 0);
        key(&mut d, Key::Home);
        assert_eq!(d.first(), 0);
    }
}

#[test]
fn limits_refuse_incomplete_requests_and_unusable_geometry() {
    let too_many = vec!["target"; 257];
    assert_eq!(
        Model::new("Confirm?", "Confirm", &too_many, 1, 9).unwrap_err(),
        Error::Limit
    );
    let long = "x".repeat(4097);
    assert_eq!(
        Model::new("Confirm?", "Confirm", &[&long], 1, 9).unwrap_err(),
        Error::Limit
    );
    for text in ["newline\nhere", "tab\there", "escape\u{1b}here"] {
        assert_eq!(
            Model::new("Confirm?", "Confirm", &[text], 1, 9).unwrap_err(),
            Error::InvalidText
        );
    }
    assert_eq!(
        Model::new("", "Confirm", &["target"], 1, 9).unwrap_err(),
        Error::InvalidText
    );
    assert_eq!(
        Model::new("Confirm?", "Confirm", &[], 1, 9).unwrap_err(),
        Error::InvalidText
    );
    let model = || Model::new("Confirm?", "Confirm", &["target"], 1, 9).unwrap();
    for rect in [
        Rect {
            width: 1,
            ..rect(1)
        },
        Rect {
            height: 95,
            ..rect(1)
        },
        Rect {
            x: i64::MIN,
            ..rect(1)
        },
        Rect {
            x: i64::MAX,
            ..rect(1)
        },
    ] {
        assert_eq!(
            Controller::new(model(), surface(1), rect, Some(42)).unwrap_err(),
            Error::NoRoom
        );
    }
    let bad = Surface {
        width: usize::MAX,
        ..surface(1)
    };
    assert_eq!(
        Controller::new(model(), bad, rect(1), Some(42)).unwrap_err(),
        Error::InvalidSurface
    );
    let max = "x".repeat(4096);
    let details = vec![max.as_str(); 256];
    let d = Controller::new(
        Model::new("Confirm?", "Confirm", &details, 1, 9).unwrap(),
        surface(1),
        rect(1),
        Some(42),
    )
    .unwrap();
    let bytes: usize = (0..d.detail_rows())
        .map(|row| d.detail_text(row).unwrap().len())
        .sum();
    assert_eq!(bytes, 1024 * 1024);
    assert!(d.detail_rows() <= td_ui::confirmations::WRAPPED_ROWS);
    let narrow = Rect {
        width: 160,
        ..rect(1)
    };
    assert_eq!(
        Controller::new(
            Model::new("Confirm?", "Confirm", &details, 1, 9).unwrap(),
            surface(1),
            narrow,
            Some(42)
        )
        .unwrap_err(),
        Error::NoRoom
    );
}

#[test]
fn fixed_controls_and_draw_stream_stay_inside_the_dialog_at_all_scales() {
    use td_ui::chrome::SELECTED_ROW;
    use td_ui::raster::{Composition, Draw, Primitive, Raster, CHROME};
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
    let face = td_ui::font::pinned().unwrap();
    for scale in 1..=4 {
        let mut d = dialog(scale);
        let surface = surface(scale);
        let mut draws = Vec::new();
        d.emit(surface.bounds(), &mut |draw| draws.push(draw));
        assert!(!draws.is_empty());
        for draw in &draws {
            assert_eq!(draw.clip.intersection(d.rect()), Some(draw.clip));
        }
        let mut bytes = vec![0x7a; surface.width * surface.height * 4];
        Raster::new(&mut bytes, &face, surface, surface.width * 4)
            .unwrap()
            .paint(&Scene { surface, draws }, surface.bounds())
            .unwrap();
        let cancel = d.action_rect(Focus::Cancel).unwrap();
        let confirm = d.action_rect(Focus::Confirm).unwrap();
        let color = |x: usize, y: usize| {
            let start = (y * surface.width + x) * 4;
            u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap()) & 0xffffff
        };
        assert_eq!(
            color(cancel.x as usize, cancel.y as usize),
            SELECTED_ROW & 0xffffff
        );
        assert_eq!(
            color(confirm.x as usize, confirm.y as usize),
            CHROME & 0xffffff
        );
        assert_eq!(color(0, 0), 0x7a7a7a);
        assert_eq!(color(surface.width - 1, surface.height - 1), 0x7a7a7a);
        assert_eq!(cancel.intersection(d.details_rect()), None);
        assert_eq!(confirm.intersection(d.details_rect()), None);
        let damage = Rect {
            x: confirm.x + 1,
            y: confirm.y + 1,
            width: 1,
            height: 1,
        };
        let mut clipped = Vec::new();
        d.emit(damage, &mut |draw| clipped.push(draw));
        assert!(clipped.iter().all(|draw| draw.clip == damage));
        assert!(clipped
            .iter()
            .any(|draw| matches!(draw.primitive, Primitive::Fill { .. })));
        key(&mut d, Key::Escape);
        let mut count = 0;
        d.emit(surface.bounds(), &mut |_| count += 1);
        assert_eq!(count, 0);
    }
}

#[test]
fn captured_details_outlive_their_source_without_following_edits() {
    let mut source = String::from("PID 12 original command");
    let model = Model::new("Suspend?", "Send SIGSTOP", &[&source], 19, 9).unwrap();
    source.clear();
    source.push_str("PID 12 replacement command");
    drop(source);
    let d = Controller::new(model, surface(1), rect(1), Some(42)).unwrap();
    assert_eq!(d.detail_text(0), Some("PID 12 original command"));
}

#[test]
fn cancelled_pointer_input_preserves_keyboard_choice_and_blank_rows_do_not_select() {
    for interrupted in [
        Event::Move { x: 0, y: 0 },
        Event::Release { x: 0, y: 0 },
        Event::Other,
    ] {
        let mut d = dialog(1);
        let confirm = d.action_rect(Focus::Confirm).unwrap();
        event(&mut d, press(confirm));
        event(&mut d, interrupted);
        assert_eq!(d.focus(), Focus::Cancel);
        assert_eq!(
            key(&mut d, Key::Activate),
            Outcome::Closed {
                choice: Choice::Cancelled,
                restore_focus: Some(42),
            }
        );
    }
    let mut d = dialog(1);
    let details = d.details_rect();
    assert_eq!(
        event(
            &mut d,
            Event::Press {
                x: details.x + 1,
                y: details.y + 5 * td_ui::chrome::ROW as i64,
            }
        ),
        Outcome::Consumed
    );
    assert_eq!(d.focus(), Focus::Cancel);
}

#[test]
fn resize_keeps_the_reading_position_and_paint_contains_every_visible_character() {
    use td_ui::raster::Primitive;
    let text = "abcdefghijklmnoλ世界".repeat(150);
    for scale in 1..=4 {
        let mut d = Controller::new(
            Model::new("Confirm?", "Send signal", &[&text], 19, 9).unwrap(),
            surface(scale),
            rect(scale),
            Some(42),
        )
        .unwrap();
        let details = d.details_rect();
        assert_eq!(
            event(
                &mut d,
                Event::Press {
                    x: details.x + 1,
                    y: details.y + 1
                }
            ),
            Outcome::Changed
        );
        assert_eq!(d.focus(), Focus::Details);
        key(&mut d, Key::PageDown);
        let first = d.first();
        assert!(first > 0);
        assert_eq!(
            event(
                &mut d,
                Event::Wheel {
                    x: 0,
                    y: 0,
                    rows: 100
                }
            ),
            Outcome::Consumed
        );
        assert_eq!(d.first(), first);
        event(
            &mut d,
            Event::Resize {
                surface: surface(scale),
                rect: rect(scale),
            },
        );
        assert_eq!(d.first(), first);
        let mut narrower = rect(scale);
        narrower.width -= 37 * u32::from(scale);
        event(
            &mut d,
            Event::Resize {
                surface: surface(scale),
                rect: narrower,
            },
        );
        assert!(d.first() > 0);
        let details = d.details_rect();
        let mut rows = std::collections::BTreeMap::<i64, String>::new();
        d.emit(surface(scale).bounds(), &mut |draw| {
            if let Primitive::Glyph { x, y, scalar, .. } = draw.primitive {
                if details.contains(x, y) {
                    rows.entry(y).or_default().push(scalar);
                }
            }
        });
        for (index, painted) in rows.values().enumerate() {
            assert_eq!(
                painted,
                &format!("  {}", d.detail_text(d.first() + index).unwrap())
            );
        }
        assert_eq!(
            rows.len(),
            details.height as usize / (td_ui::chrome::ROW * scale as usize)
        );
    }
}
