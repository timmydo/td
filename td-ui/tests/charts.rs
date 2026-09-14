#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_ui::charts::{
    Axis, Chart, Error, Event, Key, Mode, Outcome, Selection, Series, State, Time,
};
use td_ui::raster::{Primitive, Rect, Scale, Surface};
fn surface(scale: u8) -> Surface {
    Surface::new(
        700 * scale as usize,
        500 * scale as usize,
        Scale::new(scale).unwrap(),
    )
    .unwrap()
}
fn rect(scale: u8) -> Rect {
    Rect {
        x: 20 * scale as i64,
        y: 20 * scale as i64,
        width: 640 * scale as u32,
        height: 440 * scale as u32,
    }
}
fn axis() -> Axis<'static> {
    Axis {
        divisor: 1,
        decimal_places: 0,
        maximum: 100,
        maximum_label: "100",
        unit: "percent",
    }
}
const TIMES: [Time<'static>; 3] = [
    Time {
        at: 10,
        label: "00:10",
    },
    Time {
        at: 20,
        label: "00:20",
    },
    Time {
        at: 30,
        label: "00:30",
    },
];
#[test]
fn painted_series_have_matching_hits_and_gaps_have_no_identity() {
    for scale in 1..=4 {
        for mode in [Mode::Lines, Mode::Stacked] {
            let series = [
                Series {
                    id: 7,
                    label: "worker",
                    color: 0xa01010,
                    values: &[Some(20), Some(40), Some(30)],
                },
                Series {
                    id: 8,
                    label: "child",
                    color: 0x10a010,
                    values: &[Some(10), Some(10), Some(10)],
                },
            ];
            let chart =
                Chart::new(surface(scale), rect(scale), mode, axis(), &TIMES, &series).unwrap();
            let mut checked = 0;
            chart.emit(None, false, surface(scale).bounds(), &mut |draw| {
                if let Primitive::Fill { rect: paint, color } = draw.primitive {
                    if paint.width == 1 && chart.plot().contains(paint.x, paint.y) {
                        let id = match color {
                            0xa01010 => 7,
                            0x10a010 => 8,
                            _ => return,
                        };
                        // The series don't overlap in this fixture.
                        let hit = chart
                            .hit(paint.x, paint.y + paint.height as i64 / 2, None)
                            .unwrap();
                        assert_eq!(hit.series, Some(id));
                        checked += 1;
                    }
                }
            });
            assert!(checked > 0);
            let missing = [Series {
                id: 7,
                label: "gap",
                color: 0xa01010,
                values: &[Some(20), None, Some(20)],
            }];
            let chart =
                Chart::new(surface(scale), rect(scale), mode, axis(), &TIMES, &missing).unwrap();
            let plot = chart.plot();
            let hit = chart
                .hit(
                    plot.x + plot.width as i64 / 2,
                    plot.y + plot.height as i64 / 2,
                    None,
                )
                .unwrap();
            assert_eq!(
                hit,
                Selection {
                    at: 20,
                    series: None
                }
            );
        }
    }
}
#[test]
fn keyboard_and_pointer_choose_semantic_ids_and_refresh_disarms() {
    let series = [Series {
        id: 7,
        label: "worker",
        color: 0x10a010,
        values: &[Some(10); 3],
    }];
    let chart = Chart::new(surface(1), rect(1), Mode::Lines, axis(), &TIMES, &series).unwrap();
    let mut state = State::<u32, u64>::default();
    assert_eq!(
        state.event(
            &chart,
            1,
            Event::Key {
                key: Key::FirstTime,
                repeated: false
            }
        ),
        Outcome::Selected(Selection {
            at: 10,
            series: None
        })
    );
    assert_eq!(
        state.event(
            &chart,
            1,
            Event::Key {
                key: Key::NextSeries,
                repeated: false
            }
        ),
        Outcome::Selected(Selection {
            at: 10,
            series: Some(7)
        })
    );
    let plot = chart.plot();
    let x = plot.x + plot.width as i64 - 1;
    let y = plot.y;
    state.event(&chart, 1, Event::Press { x, y });
    assert_eq!(
        state.event(&chart, 2, Event::Release { x, y }),
        Outcome::Stale
    );
    assert_eq!(
        state.selection(),
        Some(Selection {
            at: 10,
            series: Some(7)
        })
    );
    state.event(&chart, 2, Event::Press { x, y });
    assert_eq!(
        state.event(&chart, 2, Event::Release { x, y }),
        Outcome::Selected(Selection {
            at: 30,
            series: None
        })
    );
    assert_eq!(
        state.event(&chart, 2, Event::Release { x, y }),
        Outcome::Ignored
    );
    state.event(&chart, 2, Event::Press { x, y });
    state.event(&chart, 2, Event::FocusLost);
    assert_eq!(
        state.event(&chart, 2, Event::Release { x, y }),
        Outcome::Ignored
    );
}
#[test]
fn extreme_timestamps_and_values_stay_inside_the_plot() {
    let times = [
        Time {
            at: 0,
            label: "start",
        },
        Time {
            at: u64::MAX,
            label: "end",
        },
    ];
    let series = [
        Series {
            id: 1,
            label: "first",
            color: 0x101010,
            values: &[Some(u64::MAX), Some(0)],
        },
        Series {
            id: 2,
            label: "second",
            color: 0x202020,
            values: &[Some(0), Some(u64::MAX)],
        },
    ];
    for scale in 1..=4 {
        let chart = Chart::new(
            surface(scale),
            rect(scale),
            Mode::Stacked,
            Axis {
                divisor: 1,
                decimal_places: 0,
                maximum: u64::MAX,
                maximum_label: "max",
                unit: "bytes",
            },
            &times,
            &series,
        )
        .unwrap();
        let plot = chart.plot();
        chart.emit(None, false, surface(scale).bounds(), &mut |draw| {
            if let Primitive::Fill {
                rect,
                color: 0x101010 | 0x202020,
            } = draw.primitive
            {
                if rect.width == 1 {
                    assert_eq!(rect.intersection(plot), Some(rect));
                }
            }
        });
        assert_eq!(
            chart.hit(plot.x, plot.y, None).unwrap(),
            Selection {
                at: 0,
                series: Some(1)
            }
        );
        assert_eq!(
            chart
                .hit(plot.x + plot.width as i64 - 1, plot.y, None)
                .unwrap(),
            Selection {
                at: u64::MAX,
                series: Some(2)
            }
        );
        assert_eq!(chart.hit(i64::MAX, i64::MIN, None), None);
    }
}
#[test]
fn validation_and_empty_history_are_explicit() {
    let series = [Series {
        id: 1,
        label: "worker",
        color: 1,
        values: &[],
    }];
    let chart = Chart::new(surface(1), rect(1), Mode::Lines, axis(), &[], &series).unwrap();
    assert_eq!(chart.hit(chart.plot().x, chart.plot().y, None), None);
    let legend = chart.legend(0).unwrap();
    assert_eq!(chart.hit(legend.x, legend.y, None), None);
    let mut state = State::<u32, u64>::default();
    assert_eq!(
        state.event(
            &chart,
            1,
            Event::Key {
                key: Key::NextTime,
                repeated: false
            }
        ),
        Outcome::Consumed
    );
    assert_eq!(
        Chart::new(
            surface(1),
            Rect {
                width: 1,
                ..rect(1)
            },
            Mode::Lines,
            axis(),
            &[],
            &series
        )
        .unwrap_err(),
        Error::NoRoom
    );
    assert_eq!(
        Chart::new(surface(1), rect(1), Mode::Lines, axis(), &TIMES, &series).unwrap_err(),
        Error::InvalidData
    );
    let duplicate = [
        Series {
            id: 1,
            label: "a",
            color: 1,
            values: &[],
        },
        Series {
            id: 1,
            label: "b",
            color: 2,
            values: &[],
        },
    ];
    assert_eq!(
        Chart::new(surface(1), rect(1), Mode::Lines, axis(), &[], &duplicate).unwrap_err(),
        Error::InvalidData
    );
}
#[test]
fn clipping_and_selected_text_have_pixel_oracles_at_every_scale() {
    use td_ui::raster::{Composition, Draw, Raster, PAPER, SELECTED};
    struct Scene<'a> {
        surface: Surface,
        chart: Chart<'a, u32>,
    }
    impl Composition for Scene<'_> {
        fn surface(&self) -> Surface {
            self.surface
        }
        fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
            self.chart.emit(
                Some(Selection {
                    at: 20,
                    series: Some(7),
                }),
                true,
                damage,
                sink,
            )
        }
    }
    let font = td_ui::font::pinned().unwrap();
    for scale in 1..=4 {
        let series = [Series {
            id: 7,
            label: "worker",
            color: 0xa01010,
            values: &[Some(10); 3],
        }];
        let surface = surface(scale);
        let chart =
            Chart::new(surface, rect(scale), Mode::Stacked, axis(), &TIMES, &series).unwrap();
        let scene = Scene { surface, chart };
        let mut glyphs = 0;
        scene.emit(surface.bounds(), &mut |draw| {
            assert_eq!(draw.clip.intersection(rect(scale)), Some(draw.clip));
            if let Primitive::Glyph { style, .. } = draw.primitive {
                if style.background == SELECTED {
                    assert_eq!(style.ink, PAPER);
                    glyphs += 1;
                }
            }
        });
        assert!(glyphs > 0);
        let mut whole = vec![0x7a; surface.width * surface.height * 4];
        Raster::new(&mut whole, &font, surface, surface.width * 4)
            .unwrap()
            .paint(&scene, surface.bounds())
            .unwrap();
        assert_eq!(&whole[..4], &[0x7a; 4]);
        let damage = Rect {
            x: rect(scale).x + 17,
            y: rect(scale).y + 23,
            width: 180,
            height: 190,
        };
        let mut partial = vec![0x7a; whole.len()];
        Raster::new(&mut partial, &font, surface, surface.width * 4)
            .unwrap()
            .paint(&scene, damage)
            .unwrap();
        for y in 0..surface.height {
            for x in 0..surface.width {
                let start = (y * surface.width + x) * 4;
                if damage.contains(x as i64, y as i64) {
                    assert_eq!(&partial[start..start + 4], &whole[start..start + 4]);
                } else {
                    assert_eq!(&partial[start..start + 4], &[0x7a; 4]);
                }
            }
        }
    }
}
#[test]
fn isolated_subpixel_observations_remain_visible_and_select_their_actual_time() {
    let times = [
        Time {
            at: 0,
            label: "zero",
        },
        Time {
            at: 4,
            label: "four",
        },
        Time {
            at: 10,
            label: "ten",
        },
    ];
    for scale in 1..=4 {
        for mode in [Mode::Lines, Mode::Stacked] {
            let series = [Series {
                id: 7,
                label: "isolated",
                color: 0xaa2222,
                values: &[None, Some(40), None],
            }];
            let chart =
                Chart::new(surface(scale), rect(scale), mode, axis(), &times, &series).unwrap();
            let mut points = Vec::new();
            chart.emit(None, false, surface(scale).bounds(), &mut |draw| {
                if let Primitive::Fill {
                    rect: point,
                    color: 0xaa2222,
                } = draw.primitive
                {
                    if point.intersection(chart.plot()) == Some(point) {
                        points.push(point);
                    }
                }
            });
            assert!(!points.is_empty());
            for point in points {
                assert_eq!(
                    chart.hit(
                        point.x + point.width as i64 / 2,
                        point.y + point.height as i64 / 2,
                        None
                    ),
                    Some(Selection {
                        at: 4,
                        series: Some(7)
                    })
                );
            }
        }
    }
    let times = [
        Time {
            at: 0,
            label: "zero",
        },
        Time {
            at: 1,
            label: "one",
        },
        Time {
            at: 1000000,
            label: "end",
        },
    ];
    let series = [Series {
        id: 7,
        label: "coincident",
        color: 0xaa2222,
        values: &[Some(40), Some(40), None],
    }];
    let chart = Chart::new(surface(1), rect(1), Mode::Stacked, axis(), &times, &series).unwrap();
    let plot = chart.plot();
    assert_eq!(
        chart.hit(plot.x, plot.y + plot.height as i64 - 2, None),
        Some(Selection {
            at: 1,
            series: Some(7)
        })
    );
}
#[test]
fn one_past_each_input_bound_and_invalid_stack_are_refused() {
    let times: Vec<Time<'_>> = (0..=td_ui::charts::SAMPLES)
        .map(|at| Time {
            at: at as u64,
            label: "sample",
        })
        .collect();
    let values = vec![Some(0); times.len()];
    let series = [Series {
        id: 0,
        label: "series",
        color: 1,
        values: &values,
    }];
    assert_eq!(
        Chart::new(surface(1), rect(1), Mode::Lines, axis(), &times, &series).unwrap_err(),
        Error::Limit
    );
    let series: Vec<Series<'_, usize>> = (0..=td_ui::charts::SERIES)
        .map(|id| Series {
            id,
            label: "series",
            color: 1,
            values: &[],
        })
        .collect();
    assert_eq!(
        Chart::new(surface(1), rect(1), Mode::Lines, axis(), &[], &series).unwrap_err(),
        Error::Limit
    );
    let series = [
        Series {
            id: 0,
            label: "a",
            color: 1,
            values: &[Some(70); 3],
        },
        Series {
            id: 1,
            label: "b",
            color: 2,
            values: &[Some(70); 3],
        },
    ];
    assert_eq!(
        Chart::new(surface(1), rect(1), Mode::Stacked, axis(), &TIMES, &series).unwrap_err(),
        Error::InvalidData
    );
    assert!(Chart::new(surface(1), rect(1), Mode::Lines, axis(), &TIMES, &series).is_ok());
    let times = [
        Time {
            at: 1,
            label: "one",
        },
        Time {
            at: 1,
            label: "duplicate",
        },
    ];
    assert_eq!(
        Chart::new(surface(1), rect(1), Mode::Lines, axis(), &times, &series).unwrap_err(),
        Error::InvalidData
    );
    let label = "a".repeat(td_ui::charts::LABEL_BYTES + 1);
    let series = [Series {
        id: 0,
        label: &label,
        color: 1,
        values: &[],
    }];
    assert_eq!(
        Chart::new(surface(1), rect(1), Mode::Lines, axis(), &[], &series).unwrap_err(),
        Error::InvalidText
    );
}
#[test]
fn admitted_maxima_bound_draw_work_and_zero_stacks_have_no_hit() {
    let label = "a".repeat(td_ui::charts::LABEL_BYTES);
    let times: Vec<Time<'_>> = (0..td_ui::charts::SAMPLES)
        .map(|at| Time {
            at: at as u64,
            label: &label,
        })
        .collect();
    let values = vec![Some(50); times.len()];
    let series: Vec<Series<'_, usize>> = (0..td_ui::charts::SERIES)
        .map(|id| Series {
            id,
            label: &label,
            color: 1,
            values: &values,
        })
        .collect();
    let surface = Surface::new(8192, 500, Scale::new(1).unwrap()).unwrap();
    for maximum_label in [&label[..], "9"] {
        let chart = Chart::new(
            surface,
            Rect {
                x: 0,
                y: 0,
                width: 8192,
                height: 400,
            },
            Mode::Lines,
            Axis {
                divisor: 1,
                decimal_places: 0,
                maximum: 50,
                maximum_label,
                unit: &label,
            },
            &times,
            &series,
        )
        .unwrap();
        let mut draws = 0;
        chart.emit(None, false, surface.bounds(), &mut |_| draws += 1);
        assert!(draws > 100000);
        assert!(draws <= td_ui::charts::DRAW_LIMIT);
    }
    let zero = [Series {
        id: 7,
        label: "zero",
        color: 1,
        values: &[Some(0); 3],
    }];
    let chart = Chart::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 600,
            height: 400,
        },
        Mode::Stacked,
        axis(),
        &TIMES,
        &zero,
    )
    .unwrap();
    let plot = chart.plot();
    for x in [
        plot.x,
        plot.x + plot.width as i64 / 2,
        plot.x + plot.width as i64 - 1,
    ] {
        assert_eq!(
            chart
                .hit(x, plot.y + plot.height as i64 - 1, None)
                .unwrap()
                .series,
            None
        );
    }
    let overflow = [
        Series {
            id: 1,
            label: "one",
            color: 1,
            values: &[Some(u64::MAX); 3],
        },
        Series {
            id: 2,
            label: "two",
            color: 2,
            values: &[Some(1); 3],
        },
    ];
    assert_eq!(
        Chart::new(
            surface,
            Rect {
                x: 0,
                y: 0,
                width: 600,
                height: 400
            },
            Mode::Stacked,
            Axis {
                divisor: 1,
                decimal_places: 0,
                maximum: u64::MAX,
                maximum_label: "max",
                unit: "bytes"
            },
            &TIMES,
            &overflow
        )
        .unwrap_err(),
        Error::InvalidData
    );
}
#[test]
fn interrupted_gestures_do_not_select_and_keyboard_covers_every_choice() {
    let series = [
        Series {
            id: 7,
            label: "one",
            color: 1,
            values: &[Some(10); 3],
        },
        Series {
            id: 8,
            label: "two",
            color: 2,
            values: &[Some(20); 3],
        },
    ];
    let chart = Chart::new(surface(1), rect(1), Mode::Lines, axis(), &TIMES, &series).unwrap();
    let plot = chart.plot();
    let x = plot.x;
    let y = plot.y;
    for interruption in [
        Event::Resize,
        Event::FocusLost,
        Event::Other,
        Event::Move { x: 0, y: 0 },
        Event::Key {
            key: Key::ClearSeries,
            repeated: true,
        },
    ] {
        let mut state = State::<u32, u64>::default();
        state.event(&chart, 1, Event::Press { x, y });
        state.event(&chart, 1, interruption);
        state.event(&chart, 1, Event::Move { x, y });
        assert_eq!(
            state.event(&chart, 1, Event::Release { x, y }),
            Outcome::Ignored
        );
        assert_eq!(state.selection(), None);
    }
    let mut state = State::<u32, u64>::default();
    for (key, expected) in [
        (
            Key::PreviousSeries,
            Selection {
                at: 30,
                series: Some(8),
            },
        ),
        (
            Key::NextSeries,
            Selection {
                at: 30,
                series: Some(7),
            },
        ),
        (
            Key::PreviousTime,
            Selection {
                at: 20,
                series: Some(7),
            },
        ),
        (
            Key::FirstTime,
            Selection {
                at: 10,
                series: Some(7),
            },
        ),
        (
            Key::LastTime,
            Selection {
                at: 30,
                series: Some(7),
            },
        ),
        (
            Key::ClearSeries,
            Selection {
                at: 30,
                series: None,
            },
        ),
    ] {
        assert_eq!(
            state.event(
                &chart,
                1,
                Event::Key {
                    key,
                    repeated: false
                }
            ),
            Outcome::Selected(expected)
        );
    }
}
#[test]
fn value_labels_scale_and_round_without_overflow_or_lost_precision() {
    for (value, divisor, decimal_places, expected) in [
        (42, 100, 2, "0.42 %"),
        (9995, 1000, 2, "10.00 %"),
        (u64::MAX, 1, 6, "18446744073709551615.000000 %"),
        (1, 3, 6, "0.333333 %"),
        (2, 3, 6, "0.666667 %"),
        (3, 2, 0, "2 %"),
    ] {
        let values = [Some(value); 3];
        let series = [Series {
            id: 7,
            label: "worker",
            color: 1,
            values: &values,
        }];
        let chart = Chart::new(
            surface(1),
            rect(1),
            Mode::Lines,
            Axis {
                maximum: u64::MAX,
                maximum_label: "max",
                unit: "%",
                divisor,
                decimal_places,
            },
            &TIMES,
            &series,
        )
        .unwrap();
        let mut label = String::new();
        chart.emit(
            Some(Selection {
                at: 20,
                series: Some(7),
            }),
            true,
            surface(1).bounds(),
            &mut |draw| {
                if let Primitive::Glyph { y, scalar, .. } = draw.primitive {
                    if y == rect(1).y + 4 {
                        label.push(scalar);
                    }
                }
            },
        );
        assert_eq!(label, expected);
    }
    let series = [Series {
        id: 7,
        label: "worker",
        color: 1,
        values: &[Some(1); 3],
    }];
    for invalid in [
        Axis {
            divisor: 0,
            ..axis()
        },
        Axis {
            decimal_places: 7,
            ..axis()
        },
    ] {
        assert_eq!(
            Chart::new(surface(1), rect(1), Mode::Lines, invalid, &TIMES, &series).unwrap_err(),
            Error::InvalidData
        );
    }
}
#[test]
fn selected_time_remains_visible_over_matching_series_colors() {
    use td_ui::raster::{Composition, Draw, Raster, INK, PAPER, SELECTED};
    struct Scene<'a> {
        surface: Surface,
        chart: Chart<'a, u32>,
    }
    impl Composition for Scene<'_> {
        fn surface(&self) -> Surface {
            self.surface
        }
        fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
            self.chart.emit(
                Some(Selection {
                    at: 20,
                    series: Some(7),
                }),
                true,
                damage,
                sink,
            )
        }
    }
    let font = td_ui::font::pinned().unwrap();
    for scale in 1..=4 {
        let surface = surface(scale);
        let series = [Series {
            id: 7,
            label: "full",
            color: SELECTED,
            values: &[Some(100); 3],
        }];
        let chart =
            Chart::new(surface, rect(scale), Mode::Stacked, axis(), &TIMES, &series).unwrap();
        let plot = chart.plot();
        let scene = Scene { surface, chart };
        let mut bytes = vec![0; surface.width * surface.height * 4];
        Raster::new(&mut bytes, &font, surface, surface.width * 4)
            .unwrap()
            .paint(&scene, surface.bounds())
            .unwrap();
        let x = plot.x as usize + (plot.width as usize - 1) / 2;
        let y = plot.y as usize + plot.height as usize / 2;
        let color = |x: usize| {
            let index = (y * surface.width + x) * 4;
            u32::from_le_bytes(bytes[index..index + 4].try_into().unwrap()) & 0xffffff
        };
        assert_eq!(color(x), INK);
        assert_eq!(color(x - 1), PAPER);
        assert_eq!(color(x + 3 * scale as usize), SELECTED);
    }
}
#[test]
fn a_rebuilt_view_cannot_retarget_a_gesture_without_a_resize_event() {
    let series = [Series {
        id: 7,
        label: "worker",
        color: 1,
        values: &[Some(20); 3],
    }];
    let chart = Chart::new(surface(1), rect(1), Mode::Lines, axis(), &TIMES, &series).unwrap();
    let mut cancelled = State::<u32, u64>::default();
    let x = chart.plot().x;
    let y = chart.plot().y;
    cancelled.event(&chart, 1, Event::Press { x, y });
    cancelled.cancel_gesture();
    assert_eq!(
        cancelled.event(&chart, 1, Event::Release { x, y }),
        Outcome::Ignored
    );
    for next in [
        Chart::new(surface(2), rect(2), Mode::Lines, axis(), &TIMES, &series).unwrap(),
        Chart::new(surface(1), rect(1), Mode::Stacked, axis(), &TIMES, &series).unwrap(),
    ] {
        let mut state = State::<u32, u64>::default();
        let x = chart.plot().x;
        let y = chart.plot().y;
        state.event(&chart, 1, Event::Press { x, y });
        assert_eq!(
            state.event(&next, 1, Event::Release { x, y }),
            Outcome::Stale
        );
        assert_eq!(state.selection(), None);
    }
}
#[test]
fn a_single_observation_has_one_centered_time_label() {
    let times = [Time {
        at: 17,
        label: "single",
    }];
    let series = [Series {
        id: 7,
        label: "worker",
        color: 1,
        values: &[Some(20)],
    }];
    for scale in 1..=4 {
        let chart = Chart::new(
            surface(scale),
            rect(scale),
            Mode::Lines,
            axis(),
            &times,
            &series,
        )
        .unwrap();
        let plot = chart.plot();
        let mut label = String::new();
        let mut first = None;
        chart.emit(None, false, surface(scale).bounds(), &mut |draw| {
            if let Primitive::Glyph { x, y, scalar, .. } = draw.primitive {
                if y == plot.y + plot.height as i64 + 4 * scale as i64 {
                    label.push(scalar);
                    first.get_or_insert(x);
                }
            }
        });
        assert_eq!(label, "single");
        assert_eq!(
            first,
            Some(plot.x + plot.width as i64 / 2 - 24 * scale as i64)
        );
    }
}

#[test]
fn a_fresh_press_replaces_a_stale_gesture() {
    let series = [Series {
        id: 7,
        label: "worker",
        color: 1,
        values: &[Some(20); 3],
    }];
    let chart = Chart::new(surface(1), rect(1), Mode::Lines, axis(), &TIMES, &series).unwrap();
    let x = chart.plot().x;
    let y = chart.plot().y;
    let mut state = State::<u32, u64>::default();
    state.event(&chart, 1, Event::Press { x, y });
    assert_eq!(
        state.event(&chart, 2, Event::Press { x, y }),
        Outcome::Consumed
    );
    assert_eq!(
        state.event(&chart, 2, Event::Release { x, y }),
        Outcome::Selected(Selection {
            at: 10,
            series: None
        })
    );
}
#[test]
fn legends_keep_valid_time_or_choose_newest_and_emit_semantic_ids() {
    for scale in 1..=4 {
        let series = [
            Series {
                id: 7,
                label: "one",
                color: 1,
                values: &[Some(10); 3],
            },
            Series {
                id: 8,
                label: "two",
                color: 2,
                values: &[Some(20); 3],
            },
        ];
        let chart = Chart::new(
            surface(scale),
            rect(scale),
            Mode::Lines,
            axis(),
            &TIMES,
            &series,
        )
        .unwrap();
        let legend = chart.legend(1).unwrap();
        for (prior, at) in [
            (None, 30),
            (
                Some(Selection {
                    at: 10,
                    series: Some(7),
                }),
                10,
            ),
            (
                Some(Selection {
                    at: 5,
                    series: Some(7),
                }),
                30,
            ),
        ] {
            let expected = Selection {
                at,
                series: Some(8),
            };
            assert_eq!(chart.hit(legend.x, legend.y, prior), Some(expected));
            let mut state = State::<u32, u64>::default();
            state.select(prior);
            state.event(
                &chart,
                1,
                Event::Press {
                    x: legend.x,
                    y: legend.y,
                },
            );
            assert_eq!(
                state.event(
                    &chart,
                    1,
                    Event::Release {
                        x: legend.x,
                        y: legend.y
                    }
                ),
                Outcome::Selected(expected)
            );
        }
    }
}
#[test]
fn a_fresh_key_survives_a_stale_gesture_and_idle_input_is_ignored() {
    let series = [Series {
        id: 7,
        label: "one",
        color: 1,
        values: &[Some(10); 3],
    }];
    let chart = Chart::new(surface(1), rect(1), Mode::Lines, axis(), &TIMES, &series).unwrap();
    let mut state = State::<u32, u64>::default();
    let plot = chart.plot();
    state.event(
        &chart,
        1,
        Event::Press {
            x: plot.x,
            y: plot.y,
        },
    );
    assert_eq!(
        state.event(
            &chart,
            2,
            Event::Key {
                key: Key::FirstTime,
                repeated: false
            }
        ),
        Outcome::Selected(Selection {
            at: 10,
            series: None
        })
    );
    for event in [
        Event::Move { x: 0, y: 0 },
        Event::FocusLost,
        Event::Resize,
        Event::Other,
    ] {
        assert_eq!(state.event(&chart, 2, event), Outcome::Ignored);
    }
}
#[test]
fn missing_timestamps_have_no_selection_marker() {
    use td_ui::raster::{INK, PAPER};
    let series = [Series {
        id: 7,
        label: "one",
        color: 1,
        values: &[Some(10); 3],
    }];
    let chart = Chart::new(surface(1), rect(1), Mode::Lines, axis(), &TIMES, &series).unwrap();
    for (at, expected) in [(15, 0), (20, 2)] {
        let mut markers = 0;
        chart.emit(
            Some(Selection {
                at,
                series: Some(7),
            }),
            false,
            surface(1).bounds(),
            &mut |draw| {
                if let Primitive::Fill { rect, color } = draw.primitive {
                    if rect.height == chart.plot().height
                        && rect.width <= 3
                        && [INK, PAPER].contains(&color)
                        && rect.intersection(chart.plot()) == Some(rect)
                    {
                        markers += 1;
                    }
                }
            },
        );
        assert_eq!(markers, expected);
    }
}
#[test]
fn short_plots_refuse_overlapping_axis_labels() {
    for scale in 1..=4 {
        let series = [Series {
            id: 7,
            label: "one",
            color: 1,
            values: &[Some(10); 3],
        }];
        let short = Rect {
            height: 143 * scale as u32,
            ..rect(scale)
        };
        assert_eq!(
            Chart::new(surface(scale), short, Mode::Lines, axis(), &TIMES, &series).unwrap_err(),
            Error::NoRoom
        );
        let enough = Rect {
            height: 144 * scale as u32,
            ..short
        };
        let chart =
            Chart::new(surface(scale), enough, Mode::Lines, axis(), &TIMES, &series).unwrap();
        assert_eq!(chart.plot().height, 48 * scale as u32);
    }
}
#[test]
fn time_navigation_repeats_step_without_activating_other_keys() {
    let series = [Series {
        id: 7,
        label: "one",
        color: 1,
        values: &[Some(10); 3],
    }];
    let chart = Chart::new(surface(1), rect(1), Mode::Lines, axis(), &TIMES, &series).unwrap();
    let mut state = State::<u32, u64>::default();
    for (key, at) in [
        (Key::PreviousTime, 20),
        (Key::PreviousTime, 10),
        (Key::PreviousTime, 10),
        (Key::NextTime, 20),
        (Key::NextTime, 30),
        (Key::NextTime, 30),
    ] {
        assert_eq!(
            state.event(
                &chart,
                1,
                Event::Key {
                    key,
                    repeated: true
                }
            ),
            Outcome::Selected(Selection { at, series: None })
        );
    }
    assert_eq!(
        state.event(
            &chart,
            1,
            Event::Key {
                key: Key::NextSeries,
                repeated: true
            }
        ),
        Outcome::Consumed
    );
    assert_eq!(
        state.selection(),
        Some(Selection {
            at: 30,
            series: None
        })
    );
}
