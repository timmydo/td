#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "asserted test fixtures"
)]

use td_editor::keys::Profile;
use td_editor::layout::{Affinity, Caret};
use td_editor::model::{Command, Selection};
use td_editor::render::Raster;
use td_editor::ui::{Controller, Event, Outcome, PointerPhase};
use td_editor::{font, replay, Error};

fn loaded(text: &str) -> Controller {
    let mut ui = Controller::default();
    // These legacy geometry fixtures exercise the optional gutter-off layout.
    ui.dispatch(Event::LineNumbers(false)).unwrap();
    ui.dispatch(Event::Load(text.as_bytes())).unwrap();
    ui
}

#[test]
fn scrollbar_pages_drags_and_cancels_without_editing() {
    for scale in 1..=4 {
        let mut ui = loaded(&"line\n".repeat(200));
        resize(&mut ui, 400 * scale as usize, 240 * scale as usize, scale);
        select(&mut ui, 1, 3);
        let before = format!("{:?}", ui.editor().document(1).unwrap());
        let bar = ui.geometry().scrollbar(201, 0).unwrap();
        let rows = ui.geometry().grid().1;
        pointer(
            &mut ui,
            PointerPhase::Press,
            bar.track.x,
            bar.track.y + i64::from(bar.track.height) - 1,
            false,
        );
        assert_eq!(ui.tab_view(1).unwrap().viewport.origin().row, rows);
        assert_eq!(
            pointer(
                &mut ui,
                PointerPhase::Release,
                bar.track.x,
                bar.track.y + i64::from(bar.track.height) - 1,
                false
            ),
            Outcome::Ignored
        );
        assert_eq!(
            pointer(&mut ui, PointerPhase::Move, 0, 0, false),
            Outcome::Ignored
        );
        let bar = ui.geometry().scrollbar(201, rows).unwrap();
        pointer(
            &mut ui,
            PointerPhase::Press,
            bar.thumb.x,
            bar.thumb.y + 2,
            true,
        );
        pointer(&mut ui, PointerPhase::Move, -100, i64::MAX, false);
        assert_eq!(ui.tab_view(1).unwrap().viewport.origin().row, 201 - rows);
        pointer(&mut ui, PointerPhase::Release, -100, i64::MIN, false);
        assert_eq!(ui.tab_view(1).unwrap().viewport.origin().row, 0);
        assert_eq!(
            pointer(&mut ui, PointerPhase::Move, 0, 200, false),
            Outcome::Ignored
        );
        assert_eq!(format!("{:?}", ui.editor().document(1).unwrap()), before);
        for event in [
            Event::CancelPointer,
            Event::Focus(false),
            Event::PromptRows(2),
            Event::LineNumbers(true),
        ] {
            let bar = ui.geometry().scrollbar(201, 0).unwrap();
            pointer(
                &mut ui,
                PointerPhase::Press,
                bar.thumb.x,
                bar.thumb.y,
                false,
            );
            ui.dispatch(event).unwrap();
            assert_eq!(
                pointer(&mut ui, PointerPhase::Move, 0, 200, false),
                Outcome::Ignored
            );
        }
    }
}

#[test]
fn scrollbar_click_does_not_quantize_origin_and_tiny_tracks_still_drag() {
    let mut ui = loaded(&"x\n".repeat(10_000));
    for height in [88, 240] {
        resize(&mut ui, 400, height, 1);
        ui.dispatch(Event::Scroll {
            tab: 1,
            revision: 0,
            rows: 137,
            columns: 0,
        })
        .unwrap();
        let origin = ui.tab_view(1).unwrap().viewport.origin().row;
        let bar = ui.geometry().scrollbar(10_001, origin).unwrap();
        assert!(bar.thumb.height < bar.track.height);
        pointer(
            &mut ui,
            PointerPhase::Press,
            bar.thumb.x,
            bar.thumb.y + 2,
            false,
        );
        pointer(
            &mut ui,
            PointerPhase::Release,
            bar.thumb.x,
            bar.thumb.y + 2,
            false,
        );
        assert_eq!(ui.tab_view(1).unwrap().viewport.origin().row, origin);
        pointer(
            &mut ui,
            PointerPhase::Press,
            bar.thumb.x,
            bar.thumb.y + 2,
            false,
        );
        pointer(&mut ui, PointerPhase::Release, bar.thumb.x, 10_000, false);
        assert_eq!(
            ui.tab_view(1).unwrap().viewport.origin().row,
            10_001 - ui.geometry().grid().1
        );
    }
}

#[test]
fn scrollbar_interior_drag_rounds_from_the_exact_original_row() {
    let mut ui = loaded(&"row\n".repeat(1000));
    resize(&mut ui, 400, 240, 1);
    ui.dispatch(Event::Scroll {
        tab: 1,
        revision: 0,
        rows: 137,
        columns: 0,
    })
    .unwrap();
    let bar = ui.geometry().scrollbar(1001, 137).unwrap();
    assert_eq!(
        (bar.track.height, bar.thumb.height, bar.thumb.y),
        (168, 24, 67)
    );
    let y = bar.thumb.y + 3;
    pointer(&mut ui, PointerPhase::Press, bar.thumb.x, y, false);
    // 991 rows over 144 pixels: round each signed delta from row 137,
    // independently of the quantized thumb position and earlier moves.
    for (delta, row) in [(0, 137), (1, 144), (7, 185), (8, 192), (72, 633), (-1, 130)] {
        pointer(&mut ui, PointerPhase::Move, 100, y + delta, false);
        assert_eq!(ui.tab_view(1).unwrap().viewport.origin().row, row);
    }
    pointer(&mut ui, PointerPhase::Release, 100, y, false);
    assert_eq!(ui.tab_view(1).unwrap().viewport.origin().row, 137);
    assert_eq!(selection(&ui), Selection::default());
}

#[test]
fn disabled_scrollbar_release_is_ignored_without_selection_or_error() {
    let mut ui = loaded("short");
    let before = format!("{:?}", ui.editor());
    let bar = ui.geometry().scrollbar(1, 0).unwrap();
    assert!(!bar.enabled());
    pointer(
        &mut ui,
        PointerPhase::Press,
        bar.thumb.x,
        bar.thumb.y,
        false,
    );
    assert_eq!(
        pointer(
            &mut ui,
            PointerPhase::Release,
            bar.thumb.x,
            bar.thumb.y,
            false
        ),
        Outcome::Ignored
    );
    assert_eq!(format!("{:?}", ui.editor()), before);
}
fn active(ui: &Controller) -> (u64, u64) {
    let id = ui.editor().active().unwrap();
    (id, ui.editor().document(id).unwrap().revision())
}

#[test]
fn horizontal_scrollbar_pages_drags_and_preserves_wrap_and_tab_state() {
    for scale in 1..=4 {
        let mut ui = loaded(&"x".repeat(1000));
        resize(&mut ui, 400 * scale as usize, 240 * scale as usize, scale);
        assert!(ui.geometry().horizontal_scrollbar(1001, 0).is_none());
        let rows = ui.geometry().grid().1;
        ui.dispatch(Event::Wrap {
            tab: 1,
            revision: 0,
            enabled: false,
        })
        .unwrap();
        assert_eq!(ui.geometry().grid().1 + 1, rows);
        let before = format!("{:?}", ui.editor().document(1).unwrap());
        let columns = ui.geometry().grid().0;
        let bar = ui.geometry().horizontal_scrollbar(1001, 0).unwrap();
        pointer(
            &mut ui,
            PointerPhase::Press,
            bar.track.x + i64::from(bar.track.width) - 1,
            bar.track.y,
            false,
        );
        assert_eq!(ui.tab_view(1).unwrap().viewport.origin().column, columns);
        assert_eq!(
            pointer(&mut ui, PointerPhase::Release, 0, 0, false),
            Outcome::Ignored
        );
        let bar = ui.geometry().horizontal_scrollbar(1001, columns).unwrap();
        pointer(
            &mut ui,
            PointerPhase::Press,
            bar.thumb.x + 2,
            bar.thumb.y,
            false,
        );
        pointer(&mut ui, PointerPhase::Move, i64::MAX, -100, false);
        assert_eq!(
            ui.tab_view(1).unwrap().viewport.origin().column,
            1001 - columns
        );
        pointer(&mut ui, PointerPhase::Release, i64::MIN, -100, false);
        assert_eq!(ui.tab_view(1).unwrap().viewport.origin().column, 0);
        assert_eq!(format!("{:?}", ui.editor().document(1).unwrap()), before);
        let state = ui.tab_view(1).unwrap();
        ui.dispatch(Event::Load(b"wrapped")).unwrap();
        assert!(ui.geometry().horizontal_scrollbar(1001, 0).is_none());
        assert_eq!(ui.tab_view(1).unwrap(), state);
        ui.dispatch(Event::SelectTab(1)).unwrap();
        assert_eq!(ui.tab_view(1).unwrap(), state);
        let bar = ui.geometry().horizontal_scrollbar(1001, 0).unwrap();
        pointer(
            &mut ui,
            PointerPhase::Press,
            bar.thumb.x + 2,
            bar.thumb.y,
            false,
        );
        assert_eq!(
            pointer(&mut ui, PointerPhase::Move, bar.thumb.x + 2, bar.thumb.y, false),
            Outcome::Changed
        );
        ui.dispatch(Event::Wrap {
            tab: 1,
            revision: 0,
            enabled: true,
        })
        .unwrap();
        assert_eq!(
            pointer(&mut ui, PointerPhase::Move, 999, 0, false),
            Outcome::Ignored
        );
        assert!(ui.geometry().horizontal_scrollbar(1001, 0).is_none());
    }
}

#[test]
fn horizontal_scrollbar_uses_exact_column_origin_and_independent_rounding() {
    let mut ui = loaded(&"x".repeat(1000));
    resize(&mut ui, 400, 240, 1);
    ui.dispatch(Event::Wrap {
        tab: 1,
        revision: 0,
        enabled: false,
    })
    .unwrap();
    ui.dispatch(Event::Scroll {
        tab: 1,
        revision: 0,
        rows: 0,
        columns: 137,
    })
    .unwrap();
    let bar = ui.geometry().horizontal_scrollbar(1001, 137).unwrap();
    assert_eq!(
        (bar.track.width, bar.thumb.width, bar.thumb.x),
        (368, 24, 57)
    );
    let x = bar.thumb.x + 3;
    pointer(&mut ui, PointerPhase::Press, x, bar.thumb.y, false);
    // 955 columns over 344 pixels, signed deltas anchored at column 137.
    for (delta, column) in [(0, 137), (1, 140), (7, 156), (172, 615), (-1, 134)] {
        pointer(&mut ui, PointerPhase::Move, x + delta, 0, false);
        assert_eq!(ui.tab_view(1).unwrap().viewport.origin().column, column);
    }
    pointer(&mut ui, PointerPhase::Release, x, 0, false);
    assert_eq!(ui.tab_view(1).unwrap().viewport.origin().column, 137);
}
fn edit(ui: &mut Controller, command: Command) {
    let (tab, revision) = active(ui);
    ui.dispatch(Event::Edit {
        tab,
        revision,
        command,
    })
    .unwrap();
}
fn key(ui: &mut Controller, chord: &str) -> Outcome {
    let (tab, revision) = active(ui);
    ui.dispatch(Event::Key {
        tab,
        revision,
        chord,
    })
    .unwrap()
}
fn select(ui: &mut Controller, anchor: usize, caret: usize) {
    edit(ui, Command::Select(Selection { anchor, caret }));
}
fn selection(ui: &Controller) -> Selection {
    ui.editor().document(active(ui).0).unwrap().selection()
}
fn pointer(ui: &mut Controller, phase: PointerPhase, x: i64, y: i64, extend: bool) -> Outcome {
    let (tab, revision) = active(ui);
    ui.dispatch(Event::Pointer {
        tab,
        revision,
        phase,
        x,
        cell_x: x,
        y,
        extend,
    })
    .unwrap()
}
fn tick(ui: &mut Controller, now: u64) {
    ui.dispatch(Event::Tick(now)).unwrap();
}
fn resize(ui: &mut Controller, width: usize, height: usize, scale: u8) {
    ui.dispatch(Event::Resize {
        width,
        height,
        scale,
    })
    .unwrap();
}
fn pixels(ui: &Controller) -> Vec<u8> {
    let geometry = ui.geometry();
    let (w, h) = geometry.dimensions();
    let mut pixels = vec![0; w * h * 4];
    let font = font::pinned().unwrap();
    Raster::new(&mut pixels, &font, geometry, w * 4)
        .unwrap()
        .paint(&ui.scene(&[]).unwrap(), geometry.bounds())
        .unwrap();
    pixels
}

#[test]
fn minibuffer_reserves_chrome_and_retains_document_pixels_and_hits() {
    for scale in 1..=4 {
        let mut ui = loaded("first\nsecond\nthird\nfourth\nfifth\nsixth\nseventh");
        resize(&mut ui, 320 * scale, 320 * scale, scale as u8);
        ui.dispatch(Event::Focus(false)).unwrap();
        let before = pixels(&ui);
        let model = format!("{:?}", ui.editor());
        let old = ui.geometry();
        let generation = ui.generation();
        ui.dispatch(Event::PromptRows(6)).unwrap();
        assert_eq!(ui.generation(), generation + 1);
        assert_eq!(format!("{:?}", ui.editor()), model);
        let geometry = ui.geometry();
        assert_eq!(geometry.prompt().y, (24 * scale) as i64);
        assert_eq!(geometry.prompt().height, (96 * scale) as u32);
        assert_eq!(geometry.tab(0, 0, 1).unwrap().y, (120 * scale) as i64);
        assert_eq!(geometry.document().y, (144 * scale) as i64);
        assert_eq!(geometry.status(), old.status());
        let shifted = pixels(&ui);
        for y in 0..7 * 16 * scale {
            for x in 8 * scale..64 * scale {
                let source = ((y + 48 * scale) * 320 * scale + x) * 4;
                let target = ((y + 144 * scale) * 320 * scale + x) * 4;
                assert_eq!(&before[source..source + 4], &shifted[target..target + 4]);
            }
        }
        pointer(&mut ui, PointerPhase::Press, 24 * scale as i64, 144 * scale as i64, false);
        assert_eq!(selection(&ui).caret, 2);
        assert_eq!(ui.dispatch(Event::PromptRows(16)), Err(Error::Limit));
        ui.dispatch(Event::PromptRows(0)).unwrap();
        assert_eq!(ui.geometry(), old);
        assert_eq!(selection(&ui).caret, 2);
        ui.dispatch(Event::PromptRows(15)).unwrap();
        resize(&mut ui, 320 * scale, 104 * scale, scale as u8);
        assert_eq!(ui.geometry().prompt().height, (32 * scale) as u32);
        assert_eq!(ui.geometry().grid().1, 0);
        assert!(ui.geometry().document().y <= ui.geometry().status().y);
    }
}

#[test]
fn line_numbers_default_toggle_digit_growth_and_hits_share_geometry() {
    for scale in 1..=4 {
        let mut ui = Controller::default();
        assert!(ui.line_numbers());
        ui.dispatch(Event::Load(b"abc\ndef\n")).unwrap();
        resize(&mut ui, 320 * scale, 240 * scale, scale as u8);
        assert_eq!(ui.geometry().gutter().width, (24 * scale) as u32);
        assert_eq!(ui.geometry().document().x, (32 * scale) as i64);
        let before = format!("{:?}", ui.editor());
        assert_eq!(
            pointer(
                &mut ui,
                PointerPhase::Press,
                (16 * scale) as i64,
                (48 * scale) as i64,
                false
            ),
            Outcome::Ignored
        );
        assert_eq!(format!("{:?}", ui.editor()), before);
        pointer(
            &mut ui,
            PointerPhase::Press,
            (40 * scale) as i64,
            (48 * scale) as i64,
            false,
        );
        assert_eq!(selection(&ui).caret, 1);
        let before = format!("{:?}", ui.editor());
        let columns = ui.geometry().grid().0;
        ui.dispatch(Event::LineNumbers(false)).unwrap();
        assert_eq!(ui.geometry().grid().0, columns + 3);
        assert_eq!(format!("{:?}", ui.editor()), before);
        let generation = ui.generation();
        assert_eq!(
            ui.dispatch(Event::LineNumbers(false)).unwrap(),
            Outcome::Ignored
        );
        assert_eq!(ui.generation(), generation);
        ui.dispatch(Event::LineNumbers(true)).unwrap();
        assert_eq!(format!("{:?}", ui.editor()), before);
        ui.dispatch(Event::Load("\n".repeat(98).as_bytes()))
            .unwrap();
        assert_eq!(ui.geometry().gutter().width, (24 * scale) as u32);
        edit(&mut ui, Command::Insert("\n".into()));
        assert_eq!(ui.geometry().gutter().width, (32 * scale) as u32);
        ui.dispatch(Event::SelectTab(1)).unwrap();
        assert_eq!(ui.geometry().gutter().width, (24 * scale) as u32);
        ui.dispatch(Event::SelectTab(2)).unwrap();
        assert_eq!(ui.geometry().gutter().width, (32 * scale) as u32);
        key(&mut ui, "C-Tab");
        assert_eq!(ui.editor().active(), Some(1));
        assert_eq!(ui.geometry().gutter().width, (24 * scale) as u32);
        key(&mut ui, "C-S-Tab");
        assert_eq!(ui.editor().active(), Some(2));
        assert_eq!(ui.geometry().gutter().width, (32 * scale) as u32);
        let first = ui.geometry().tab(0, 1, 2).unwrap();
        pointer(
            &mut ui,
            PointerPhase::Press,
            first.x + 1,
            first.y + 1,
            false,
        );
        assert_eq!(ui.editor().active(), Some(1));
        assert_eq!(ui.geometry().gutter().width, (24 * scale) as u32);
        let second = ui.geometry().tab(1, 0, 2).unwrap();
        pointer(
            &mut ui,
            PointerPhase::Press,
            second.x + 1,
            second.y + 1,
            false,
        );
        assert_eq!(ui.editor().active(), Some(2));
        assert_eq!(ui.geometry().gutter().width, (32 * scale) as u32);
        assert_eq!(
            ui.geometry().grid(),
            ui.tab_view(2).unwrap().viewport.dimensions()
        );
        edit(&mut ui, Command::Undo);
        assert_eq!(ui.geometry().gutter().width, (24 * scale) as u32);
        ui.dispatch(Event::SelectTab(1)).unwrap();
        assert!(ui.line_numbers());
        assert_eq!(ui.geometry().grid().0, columns);
        resize(&mut ui, 1, 1, scale as u8);
        assert_eq!(ui.geometry().document().width, 0);
        assert_eq!(pixels(&ui).len(), 4);
    }
}

#[test]
fn line_number_glyphs_skip_wrapped_rows_and_partial_damage_matches_full_frame() {
    use td_editor::render::{Draw, Primitive, Rect, LINE_NUMBER};
    for scale in 1..=4 {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(b"abcde\nx\n")).unwrap();
        resize(&mut ui, 88 * scale, 168 * scale, scale as u8); // Four text cells.
        let mut numbers = Vec::new();
        ui.scene(&[])
            .unwrap()
            .emit(ui.geometry().bounds(), &mut |draw: Draw| {
                if let Primitive::Glyph {
                    x,
                    y,
                    scalar,
                    style,
                } = draw.primitive
                {
                    if style.ink == LINE_NUMBER {
                        numbers.push((x, y, scalar));
                    }
                }
            });
        assert_eq!(
            numbers,
            vec![
                (16 * scale as i64, 48 * scale as i64, '1'),
                (16 * scale as i64, 80 * scale as i64, '2'),
                (16 * scale as i64, 96 * scale as i64, '3')
            ]
        );
        let full = pixels(&ui);
        let geometry = ui.geometry();
        let (width, height) = geometry.dimensions();
        let font = font::pinned().unwrap();
        let mut tiled = vec![0; full.len()];
        let mut raster = Raster::new(&mut tiled, &font, geometry, width * 4).unwrap();
        for x in (0..width).step_by(13) {
            raster
                .paint(
                    &ui.scene(&[]).unwrap(),
                    Rect {
                        x: x as i64,
                        y: 0,
                        width: 13,
                        height: height as u32,
                    },
                )
                .unwrap();
        }
        assert_eq!(tiled, full);
        resize(&mut ui, 88 * scale, 104 * scale, scale as u8);
        ui.dispatch(Event::Scroll {
            tab: 1,
            revision: 0,
            rows: 1,
            columns: 0,
        })
        .unwrap();
        assert_eq!(ui.tab_view(1).unwrap().viewport.origin().row, 1);
        numbers.clear();
        ui.scene(&[])
            .unwrap()
            .emit(ui.geometry().bounds(), &mut |draw: Draw| {
                if let Primitive::Glyph {
                    x,
                    y,
                    scalar,
                    style,
                } = draw.primitive
                {
                    if style.ink == LINE_NUMBER {
                        numbers.push((x, y, scalar));
                    }
                }
            });
        assert_eq!(numbers, vec![(16 * scale as i64, 64 * scale as i64, '2')]);
        ui.dispatch(Event::Wrap {
            tab: 1,
            revision: 0,
            enabled: false,
        })
        .unwrap();
        ui.dispatch(Event::Scroll {
            tab: 1,
            revision: 0,
            rows: 0,
            columns: 1,
        })
        .unwrap();
        assert_eq!(ui.tab_view(1).unwrap().viewport.origin().column, 1);
        numbers.clear();
        ui.scene(&[])
            .unwrap()
            .emit(ui.geometry().bounds(), &mut |draw: Draw| {
                if let Primitive::Glyph {
                    x,
                    y,
                    scalar,
                    style,
                } = draw.primitive
                {
                    if style.ink == LINE_NUMBER {
                        numbers.push((x, y, scalar));
                    }
                }
            });
        assert_eq!(numbers.first().unwrap().0, 16 * scale as i64);
        assert_eq!(ui.editor().document(1).unwrap().revision(), 0);
    }
}

#[test]
fn cancel_input_preserves_document_focus_and_selection_but_ends_prefix_mark_drag() {
    let mut ui = loaded("abcdef");
    ui.dispatch(Event::Profile(Profile::Emacs)).unwrap();
    select(&mut ui, 1, 4);
    key(&mut ui, "C-x");
    let before = format!("{:?}", ui.editor());
    assert_eq!(ui.dispatch(Event::CancelInput).unwrap(), Outcome::Changed);
    assert_eq!(format!("{:?}", ui.editor()), before);
    assert!(!ui.keys().pending());
    assert!(ui.focused());
    assert_eq!(ui.dispatch(Event::CancelInput).unwrap(), Outcome::Ignored);
    key(&mut ui, "C-Space");
    ui.dispatch(Event::CancelInput).unwrap();
    key(&mut ui, "Right");
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 5,
            caret: 5
        }
    );
    pointer(&mut ui, PointerPhase::Press, 8, 48, false);
    ui.dispatch(Event::CancelInput).unwrap();
    assert_eq!(
        pointer(&mut ui, PointerPhase::Move, 48, 48, false),
        Outcome::Ignored
    );
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 0,
            caret: 0
        }
    );
}

#[test]
fn vertical_motion_retains_desired_column_and_shift_selection() {
    let mut ui = loaded("abcdef\nx\nabcdef");
    select(&mut ui, 5, 5);
    key(&mut ui, "Down");
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 8,
            caret: 8
        }
    );
    assert_eq!(ui.tab_view(1).unwrap().desired_column, Some(5));
    key(&mut ui, "Down");
    assert_eq!(selection(&ui).caret, 14);
    key(&mut ui, "S-Up");
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 14,
            caret: 8
        }
    );
    key(&mut ui, "S-Up");
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 14,
            caret: 5
        }
    );
    key(&mut ui, "Left");
    assert_eq!(ui.tab_view(1).unwrap().desired_column, None);
    assert_eq!(ui.editor().document(1).unwrap().revision(), 0);
    assert!(!ui.editor().document(1).unwrap().dirty());
}

#[test]
fn height_only_resize_preserves_vertical_column_and_metrics() {
    let mut ui = loaded("abcdef\nx\nabcdef");
    select(&mut ui, 5, 5);
    key(&mut ui, "Down");
    let before = ui.tab_view(1).unwrap();
    resize(&mut ui, 800, 400, 1);
    let after = ui.tab_view(1).unwrap();
    assert_eq!(after.metrics, before.metrics);
    assert_eq!(after.desired_column, Some(5));
    key(&mut ui, "Down");
    assert_eq!(selection(&ui).caret, 14);
}

#[test]
fn page_motion_and_emacs_mark_share_the_same_selection_path() {
    let mut ui = loaded("abc\nabc\nabc\nabc\nabc\nabc\n");
    resize(&mut ui, 96, 104, 1); // two rows
    select(&mut ui, 2, 2);
    key(&mut ui, "S-PageDown");
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 2,
            caret: 10
        }
    );
    key(&mut ui, "S-PageUp");
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 2,
            caret: 2
        }
    );
    ui.dispatch(Event::Profile(Profile::Emacs)).unwrap();
    key(&mut ui, "C-Space");
    key(&mut ui, "C-n");
    key(&mut ui, "C-n");
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 2,
            caret: 10
        }
    );
    key(&mut ui, "Z");
    assert_eq!(
        ui.editor().document(1).unwrap().text(),
        "abZc\nabc\nabc\nabc\n"
    );
    assert_eq!(ui.tab_view(1).unwrap().desired_column, None);
    key(&mut ui, "C-p");
    assert_eq!(selection(&ui).anchor, selection(&ui).caret);
}

#[test]
fn pointer_scaling_soft_affinity_and_vertical_motion_use_one_layout() {
    for scale in 1..=4 {
        let s = i64::from(scale);
        let mut ui = loaded("abcdefgh");
        resize(&mut ui, 64 * scale as usize, 104 * scale as usize, scale);
        pointer(&mut ui, PointerPhase::Press, 39 * s, 49 * s, false);
        assert_eq!(selection(&ui).caret, 4);
        assert_eq!(ui.tab_view(1).unwrap().affinity, Affinity::Upstream);
        key(&mut ui, "Down");
        assert_eq!(selection(&ui).caret, 8);
        key(&mut ui, "Up");
        assert_eq!(selection(&ui).caret, 4);
        assert_eq!(ui.tab_view(1).unwrap().affinity, Affinity::Upstream);
        pointer(&mut ui, PointerPhase::Press, 8 * s, 65 * s, false);
        assert_eq!(selection(&ui).caret, 4);
        assert_eq!(ui.tab_view(1).unwrap().affinity, Affinity::Downstream);
    }
}

#[test]
fn scaled_pointer_midpoints_use_physical_pixels_without_rounding_bias() {
    for scale in 1..=4 {
        let mut ui = loaded("a\tλ\n");
        let s = i64::from(scale);
        resize(&mut ui, 112 * scale as usize, 104 * scale as usize, scale);
        for (midpoint, before, after) in [(12, 0, 1), (44, 1, 2), (76, 2, 4)] {
            pointer(&mut ui, PointerPhase::Press, midpoint * s, 49 * s, false);
            assert_eq!(selection(&ui).caret, before);
            pointer(
                &mut ui,
                PointerPhase::Press,
                midpoint * s + 1,
                49 * s,
                false,
            );
            assert_eq!(
                selection(&ui).caret,
                after,
                "scale {scale}, midpoint {midpoint}"
            );
        }
    }
}

#[test]
fn repeated_clicks_select_unicode_words_then_logical_lines() {
    let mut ui = loaded("alpha βeta!\nnext");
    resize(&mut ui, 160, 104, 1);
    let x = 58;
    let y = 49;

    pointer(&mut ui, PointerPhase::Press, x, y, false);
    pointer(&mut ui, PointerPhase::Release, x, y, false);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 6,
            caret: 6
        }
    );

    tick(&mut ui, 100);
    pointer(&mut ui, PointerPhase::Press, x, y, false);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 6,
            caret: 11
        }
    );
    pointer(&mut ui, PointerPhase::Release, x, y, false);

    tick(&mut ui, 200);
    pointer(&mut ui, PointerPhase::Press, x, y, false);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 0,
            caret: 13
        }
    );
    pointer(&mut ui, PointerPhase::Release, x, y, false);

    // A fourth click starts a new sequence, and the timeout does too.
    tick(&mut ui, 300);
    pointer(&mut ui, PointerPhase::Press, x, y, false);
    pointer(&mut ui, PointerPhase::Release, x, y, false);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 6,
            caret: 6
        }
    );
    tick(&mut ui, 801);
    pointer(&mut ui, PointerPhase::Press, x, y, false);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 6,
            caret: 6
        }
    );
}

#[test]
fn click_sequences_use_cell_pixels_and_end_on_motion_or_other_presses() {
    let mut ui = loaded("a b");
    resize(&mut ui, 224, 208, 2);
    pointer(&mut ui, PointerPhase::Press, 31, 98, false);
    pointer(&mut ui, PointerPhase::Release, 31, 98, false);
    tick(&mut ui, 100);
    pointer(&mut ui, PointerPhase::Press, 31, 98, false);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 0,
            caret: 1
        }
    );

    let mut ui = loaded("alpha beta");
    pointer(&mut ui, PointerPhase::Press, 10, 49, false);
    pointer(&mut ui, PointerPhase::Release, 10, 49, false);
    pointer(&mut ui, PointerPhase::Move, 100, 49, false);
    pointer(&mut ui, PointerPhase::Move, 10, 49, false);
    tick(&mut ui, 100);
    pointer(&mut ui, PointerPhase::Press, 10, 49, false);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 0,
            caret: 0
        }
    );

    let mut ui = Controller::default();
    ui.dispatch(Event::Load(b"alpha")).unwrap();
    let area = ui.geometry().document();
    pointer(&mut ui, PointerPhase::Press, area.x, area.y, false);
    pointer(&mut ui, PointerPhase::Release, area.x, area.y, false);
    pointer(
        &mut ui,
        PointerPhase::Press,
        area.x.saturating_sub(1),
        area.y,
        false,
    );
    pointer(&mut ui, PointerPhase::Press, area.x, area.y, false);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 0,
            caret: 0
        }
    );
}

#[test]
fn dragging_is_scalar_aligned_clamped_and_cancelled_by_edits_and_focus() {
    let mut ui = loaded("éλxyz\nlast");
    resize(&mut ui, 96, 104, 1);
    pointer(&mut ui, PointerPhase::Press, 8, 49, false);
    pointer(&mut ui, PointerPhase::Move, 21, 49, false);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 0,
            caret: 4
        }
    );
    pointer(&mut ui, PointerPhase::Release, i64::MAX, i64::MAX, false);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 0,
            caret: 12
        }
    );
    assert_eq!(
        pointer(&mut ui, PointerPhase::Move, 8, 49, false),
        Outcome::Ignored
    );
    select(&mut ui, 2, 2);
    pointer(&mut ui, PointerPhase::Press, 8, 49, true);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 2,
            caret: 0
        }
    );
    key(&mut ui, "Z");
    assert_eq!(
        pointer(&mut ui, PointerPhase::Move, 32, 49, false),
        Outcome::Ignored
    );
    pointer(&mut ui, PointerPhase::Press, 8, 49, false);
    ui.dispatch(Event::Focus(false)).unwrap();
    ui.dispatch(Event::Focus(true)).unwrap();
    assert_eq!(
        pointer(&mut ui, PointerPhase::Move, 32, 49, false),
        Outcome::Ignored
    );
    pointer(&mut ui, PointerPhase::Press, 8, 49, false);
    pointer(&mut ui, PointerPhase::Release, i64::MIN, i64::MIN, false);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 0,
            caret: 0
        }
    );
}

#[test]
fn scrolling_preserves_selection_and_tab_origins_until_a_caret_reveal() {
    let mut ui = loaded("0123456789\nabcdefghij\nklmnopqrst\nuvwxyz\nlast");
    resize(&mut ui, 64, 104, 1);
    ui.dispatch(Event::Wrap {
        tab: 1,
        revision: 0,
        enabled: false,
    })
    .unwrap();
    ui.dispatch(Event::Scroll {
        tab: 1,
        revision: 0,
        rows: isize::MAX,
        columns: isize::MAX,
    })
    .unwrap();
    let state = ui.tab_view(1).unwrap();
    assert_eq!(state.viewport.origin().row, 4);
    assert_eq!(state.viewport.origin().column, 7);
    assert_eq!(selection(&ui), Selection::default());
    ui.dispatch(Event::Load(b"second")).unwrap();
    ui.dispatch(Event::SelectTab(1)).unwrap();
    assert_eq!(ui.tab_view(1).unwrap(), state);
    key(&mut ui, "Right");
    assert_eq!(ui.tab_view(1).unwrap().viewport.origin().row, 0);
    assert_eq!(ui.tab_view(1).unwrap().viewport.origin().column, 1);
    ui.dispatch(Event::Wrap {
        tab: 1,
        revision: 0,
        enabled: true,
    })
    .unwrap();
    assert_eq!(ui.tab_view(1).unwrap().viewport.origin().column, 0);
    assert!(!ui.editor().document(1).unwrap().dirty());
}

#[test]
fn pointer_scroll_tabs_close_targets_and_tiny_chrome_match_the_renderer() {
    let mut ui = loaded("0123456789");
    resize(&mut ui, 48, 104, 1);
    ui.dispatch(Event::Wrap {
        tab: 1,
        revision: 0,
        enabled: false,
    })
    .unwrap();
    ui.dispatch(Event::Scroll {
        tab: 1,
        revision: 0,
        rows: 0,
        columns: 4,
    })
    .unwrap();
    pointer(&mut ui, PointerPhase::Press, 8, 49, false);
    assert_eq!(selection(&ui).caret, 4);
    resize(&mut ui, 800, 600, 1);
    ui.dispatch(Event::Load(b"second")).unwrap();
    assert_eq!(
        pointer(&mut ui, PointerPhase::Press, 145, 30, false),
        Outcome::Request {
            name: "close-tab",
            tab: 1,
            revision: 0
        }
    );
    assert_eq!(ui.editor().active(), Some(2));
    pointer(&mut ui, PointerPhase::Press, 20, 30, false);
    assert_eq!(ui.editor().active(), Some(1));
    resize(&mut ui, 800, 40, 1);
    assert_eq!(
        pointer(&mut ui, PointerPhase::Press, 180, 30, false),
        Outcome::Ignored
    );
    assert_eq!(ui.editor().active(), Some(1));
}

#[test]
fn clamped_scroll_does_not_cancel_a_selection_drag() {
    let mut ui = loaded("abcdefgh");
    pointer(&mut ui, PointerPhase::Press, 8, 49, false);
    pointer(&mut ui, PointerPhase::Move, 21, 49, false);
    let generation = ui.generation();
    assert_eq!(
        ui.dispatch(Event::Scroll {
            tab: 1,
            revision: 0,
            rows: -1,
            columns: 0
        })
        .unwrap(),
        Outcome::Ignored
    );
    assert_eq!(ui.generation(), generation);
    pointer(&mut ui, PointerPhase::Release, 37, 49, false);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 0,
            caret: 4
        }
    );
}

#[test]
fn every_admitted_width_and_maximum_height_fit_the_layout_limits() {
    use td_editor::layout::{Viewport, MAX_COLUMNS, MAX_ROWS};
    use td_editor::render::{Geometry, Scale, MAX_AXIS, MAX_FRAME_BYTES};
    let ui = loaded("a\nb");
    let doc = ui.editor().document(1).unwrap();
    for scale in 1..=4 {
        for width in 1..=MAX_AXIS {
            // Grid dimensions are monotonic, so the largest permitted height
            // covers the bound for every height at this width and scale.
            let height = MAX_AXIS.min(MAX_FRAME_BYTES / 4 / width);
            let geometry = Geometry::new(width, height, Scale::new(scale).unwrap()).unwrap();
            let (columns, rows) = geometry.grid();
            assert!(columns <= MAX_COLUMNS && rows <= MAX_ROWS);
            Viewport::new(columns.max(1), rows.max(1))
                .unwrap()
                .layout(doc, true)
                .unwrap();
        }
    }
}

#[test]
fn tiny_resizes_preserve_text_and_invalid_geometry_preserves_state() {
    let mut ui = loaded("a\nb\nc");
    for (w, h) in [(1, 1), (8192, 1), (1, 8192), (17, 73), (800, 600)] {
        resize(&mut ui, w, h, 1);
        let _ = pixels(&ui);
        assert_eq!(ui.editor().document(1).unwrap().text(), "a\nb\nc");
    }
    let geometry = ui.geometry();
    let state = ui.tab_view(1).unwrap();
    let generation = ui.generation();
    for (width, height, scale) in [(0, 600, 1), (8193, 1, 1), (8192, 8192, 1), (800, 600, 0)] {
        assert!(ui
            .dispatch(Event::Resize {
                width,
                height,
                scale
            })
            .is_err());
        assert_eq!(ui.geometry(), geometry);
        assert_eq!(ui.tab_view(1).unwrap(), state);
        assert_eq!(ui.generation(), generation);
    }
    resize(&mut ui, 1, 1, 4);
    assert_eq!(
        ui.dispatch(Event::Key {
            tab: 1,
            revision: 0,
            chord: "Down"
        }),
        Err(Error::Unavailable)
    );
    assert_eq!(
        pointer(&mut ui, PointerPhase::Press, 0, 0, false),
        Outcome::Ignored
    );
}

#[test]
fn an_unfocused_click_can_select_before_keyboard_focus_arrives() {
    let mut ui = loaded("abcdef");
    ui.dispatch(Event::Focus(false)).unwrap();
    pointer(&mut ui, PointerPhase::Press, 8, 49, false);
    ui.dispatch(Event::Focus(true)).unwrap();
    pointer(&mut ui, PointerPhase::Release, 29, 49, false);
    assert_eq!(
        selection(&ui),
        Selection {
            anchor: 0,
            caret: 3
        }
    );
}

#[test]
fn stale_failed_and_unfocused_events_leave_document_and_prefix_intact() {
    let mut ui = loaded("abc");
    ui.dispatch(Event::Profile(Profile::Emacs)).unwrap();
    key(&mut ui, "C-x");
    let before = ui.generation();
    for event in [
        Event::Key {
            tab: 1,
            revision: 1,
            chord: "C-s",
        },
        Event::Key {
            tab: 1,
            revision: 0,
            chord: "invalid",
        },
        Event::Edit {
            tab: 1,
            revision: 1,
            command: Command::Insert("Z".into()),
        },
        Event::Scroll {
            tab: 1,
            revision: 1,
            rows: 1,
            columns: 0,
        },
        Event::Pointer {
            tab: 1,
            revision: 1,
            phase: PointerPhase::Press,
            x: 8,
            cell_x: 8,
            y: 49,
            extend: false,
        },
    ] {
        assert!(ui.dispatch(event).is_err());
        assert_eq!(ui.generation(), before);
        assert!(ui.keys().pending());
        assert_eq!(ui.editor().document(1).unwrap().text(), "abc");
    }
    assert_eq!(
        key(&mut ui, "C-s"),
        Outcome::Request {
            name: "save",
            tab: 1,
            revision: 0
        }
    );
    key(&mut ui, "C-x");
    ui.dispatch(Event::Focus(false)).unwrap();
    assert!(!ui.keys().pending());
    let before = ui.generation();
    assert_eq!(
        ui.dispatch(Event::Key {
            tab: 1,
            revision: 0,
            chord: "Z"
        }),
        Err(Error::Unavailable)
    );
    assert_eq!(ui.generation(), before);
    ui.dispatch(Event::Focus(true)).unwrap();
    key(&mut ui, "Z");
    assert_eq!(
        ui.dispatch(Event::Close {
            tab: 1,
            revision: 1
        }),
        Err(Error::Dirty)
    );
    assert_eq!(ui.editor().document(1).unwrap().text(), "Zabc");
}

#[test]
fn a_tick_before_input_restarts_a_complete_visible_blink_phase() {
    let mut ui = loaded("abc");
    ui.dispatch(Event::Tick(450)).unwrap();
    key(&mut ui, "Right");
    let visible = pixels(&ui);
    ui.dispatch(Event::Tick(500)).unwrap();
    assert_eq!(pixels(&ui), visible);
    ui.dispatch(Event::Tick(949)).unwrap();
    assert_eq!(pixels(&ui), visible);
    ui.dispatch(Event::Tick(950)).unwrap();
    assert_ne!(pixels(&ui), visible);
}

#[test]
fn explicit_clock_and_focus_drive_pixels_without_document_edits() {
    let mut ui = loaded("abc");
    let on = pixels(&ui);
    let generation = ui.generation();
    assert_eq!(ui.dispatch(Event::Tick(499)).unwrap(), Outcome::Ignored);
    assert_eq!(ui.generation(), generation);
    ui.dispatch(Event::Tick(500)).unwrap();
    let off = pixels(&ui);
    assert_ne!(on, off);
    ui.dispatch(Event::Tick(1000)).unwrap();
    assert_eq!(pixels(&ui), on);
    let generation = ui.generation();
    assert_eq!(ui.dispatch(Event::Tick(999)), Err(Error::InvalidArgument));
    assert_eq!(ui.generation(), generation);
    ui.dispatch(Event::Focus(false)).unwrap();
    assert_eq!(pixels(&ui), off);
    ui.dispatch(Event::Tick(u64::MAX)).unwrap();
    ui.dispatch(Event::Focus(true)).unwrap();
    assert_eq!(pixels(&ui), on);
    assert_eq!(ui.editor().document(1).unwrap().revision(), 0);
}

#[test]
fn replay_and_typed_events_produce_identical_state_and_pixels() {
    let mut session = replay::Session::default();
    let mut direct = Controller::default();
    let source = "abcde\nx\nabcde";
    let commands = [
        format!("load\t{}", replay::hex(source.as_bytes())),
        "resize\t64\t104\t1".into(),
        "select-range\t1\t0\t4\t4".into(),
        format!("key\t1\t0\t{}", replay::hex(b"Down")),
        "pointer\t1\t0\tpress\t8\t49\t0".into(),
        "pointer\t1\t0\trelease\t32\t65\t0".into(),
        "pointer\t1\t0\tpress\t24\t49\t0".into(),
        "pointer\t1\t0\trelease\t0\t0\t0".into(),
        "set-soft-wrap\t1\t0\t0".into(),
        "scroll\t1\t0\trows\tforward\t2".into(),
        "tick\t500".into(),
    ];
    let events = [
        Event::Load(source.as_bytes()),
        Event::Resize {
            width: 64,
            height: 104,
            scale: 1,
        },
        Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Select(Selection {
                anchor: 4,
                caret: 4,
            }),
        },
        Event::Key {
            tab: 1,
            revision: 0,
            chord: "Down",
        },
        Event::Pointer {
            tab: 1,
            revision: 0,
            phase: PointerPhase::Press,
            x: 8,
            cell_x: 8,
            y: 49,
            extend: false,
        },
        Event::Pointer {
            tab: 1,
            revision: 0,
            phase: PointerPhase::Release,
            x: 32,
            cell_x: 32,
            y: 65,
            extend: false,
        },
        Event::Pointer {
            tab: 1,
            revision: 0,
            phase: PointerPhase::Press,
            x: 24,
            cell_x: 24,
            y: 49,
            extend: false,
        },
        Event::Pointer {
            tab: 1,
            revision: 0,
            phase: PointerPhase::Release,
            x: i64::MIN,
            cell_x: i64::MIN,
            y: i64::MIN,
            extend: false,
        },
        Event::Wrap {
            tab: 1,
            revision: 0,
            enabled: false,
        },
        Event::Scroll {
            tab: 1,
            revision: 0,
            rows: 2,
            columns: 0,
        },
        Event::Tick(500),
    ];
    assert_eq!(commands.len(), events.len());
    for (command, event) in commands.iter().zip(events) {
        assert!(!session
            .request(format!("1\t7\t{command}").as_bytes())
            .contains("error"));
        direct.dispatch(event).unwrap();
        assert_eq!(session.ui.generation(), direct.generation());
        assert_eq!(session.ui.tab_view(1).unwrap(), direct.tab_view(1).unwrap());
        assert_eq!(selection(&session.ui), selection(&direct));
        assert_eq!(pixels(&session.ui), pixels(&direct));
    }
}

#[test]
fn generated_events_keep_view_metrics_clamped_and_model_valid() {
    let mut ui = loaded("éλ\n0123456789\na\tb\nlast");
    let mut random = 73u32;
    for _ in 0..500 {
        random = random.wrapping_mul(1664525).wrapping_add(1013904223);
        let (tab, revision) = active(&ui);
        match random % 10 {
            0 => {
                key(&mut ui, "x");
            }
            1 => {
                key(&mut ui, "Backspace");
            }
            2 => {
                key(&mut ui, "Down");
            }
            3 => {
                key(&mut ui, "S-Up");
            }
            4 => {
                key(&mut ui, "Right");
            }
            5 => {
                resize(
                    &mut ui,
                    48 + (random as usize % 60),
                    104 + (random as usize % 60),
                    1,
                );
            }
            6 => {
                ui.dispatch(Event::Scroll {
                    tab,
                    revision,
                    rows: (random % 7) as isize - 3,
                    columns: 1,
                })
                .unwrap();
            }
            7 => {
                ui.dispatch(Event::Wrap {
                    tab,
                    revision,
                    enabled: random & 16 == 0,
                })
                .unwrap();
            }
            8 => {
                pointer(
                    &mut ui,
                    PointerPhase::Press,
                    8 + (random % 30) as i64,
                    49,
                    false,
                );
            }
            _ => {
                key(&mut ui, "C-Home");
            }
        }
        let doc = ui.editor().document(tab).unwrap();
        let state = ui.tab_view(tab).unwrap();
        let layout = state.viewport.layout(doc, state.soft_wrap).unwrap();
        assert_eq!(state.metrics, layout.metrics());
        assert_eq!(state.revision, doc.revision());
        assert!(doc.text().is_char_boundary(doc.selection().caret));
        assert!(doc.text().is_char_boundary(doc.selection().anchor));
        assert!(layout
            .position(Caret {
                byte: doc.selection().caret,
                affinity: state.affinity
            })
            .is_ok());
        assert!(
            state.viewport.origin().row
                <= state
                    .metrics
                    .rows
                    .saturating_sub(state.viewport.dimensions().1)
        );
        if state.soft_wrap {
            assert_eq!(state.viewport.origin().column, 0);
        } else {
            assert!(
                state.viewport.origin().column
                    <= state
                        .metrics
                        .columns
                        .saturating_sub(state.viewport.dimensions().0)
            );
        }
    }
}

#[test]
fn malformed_ui_wire_commands_do_not_change_state() {
    let mut session = replay::Session::default();
    session.request(b"1\t0\tload\t616263");
    let before = session.request(b"1\t0\tstate");
    for command in [
        "resize\t0\t600\t1",
        "resize\t800\t600\t256",
        "focus\t2",
        "tick\t-1",
        "tick\t18446744073709551616",
        "pointer\t1\t0\tclick\t8\t49\t0",
        "pointer\t1\t0\tpress\t18446744073709551615\t49\t0",
        "pointer\t1\t0\tpress\t8\t49\t0\textra",
        "set-soft-wrap\t1\t0\t2",
        "scroll\t1\t0\trows\tnegative\t1",
        "scroll\t1\t0\trows\tforward\t18446744073709551615",
    ] {
        assert!(
            session
                .request(format!("1\t0\t{command}").as_bytes())
                .contains("error"),
            "{command}"
        );
        assert_eq!(session.request(b"1\t0\tstate"), before, "{command}");
    }
}

#[test]
fn the_real_replay_binary_accepts_ui_events_without_a_display() {
    use std::io::Write;
    use std::process::{Command as Process, Stdio};
    let requests = [
        "1\t1\tload\t6162630a78797a",
        "1\t2\tresize\t64\t104\t1",
        "1\t3\tkey\t1\t0\t446f776e",
        "1\t4\tpointer\t1\t0\tpress\t8\t49\t0",
        "1\t5\ttick\t500",
        "1\t6\tstate",
    ];
    let mut wire = Vec::new();
    let mut expected = Vec::new();
    let mut session = replay::Session::default();
    for request in requests {
        wire.extend_from_slice(&(request.len() as u32).to_be_bytes());
        wire.extend_from_slice(request.as_bytes());
        let response = session.request(request.as_bytes());
        assert!(!response.contains("error"));
        expected.extend_from_slice(&(response.len() as u32).to_be_bytes());
        expected.extend_from_slice(response.as_bytes());
    }
    let mut child = Process::new(env!("CARGO_BIN_EXE_td-editor"))
        .arg("--replay")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&wire).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, expected);
}
