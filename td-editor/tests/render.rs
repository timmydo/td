use td_editor::font::{self, Font};
use td_editor::keys::Profile;
use td_editor::layout::{Affinity, Position};
use td_editor::model::{Command, Editor, Selection};
use td_editor::render::{self, Draw, Geometry, Label, Primitive, Raster, Rect, Scale, Scene, View};
use td_editor::Error;

#[allow(clippy::unwrap_used, reason = "bounded test geometry")]
fn geometry(width: usize, height: usize, scale: u8) -> Geometry {
    Geometry::new(width, height, Scale::new(scale).unwrap()).unwrap()
}

#[allow(clippy::unwrap_used, reason = "valid text fixture")]
fn editor(input: &str, selection: Selection) -> Editor {
    let mut editor = Editor::default();
    let id = editor.load_bytes(input.as_bytes()).unwrap();
    editor.dispatch(id, 0, Command::Select(selection)).unwrap();
    editor
}

#[allow(clippy::unwrap_used, reason = "validated test scene")]
fn pixels(editor: &Editor, geometry: Geometry, view: View) -> Vec<u8> {
    let font = font::pinned().unwrap();
    let (w, h) = geometry.dimensions();
    let mut pixels = vec![0xaa; w * h * 4];
    let scene = Scene::new(editor, geometry, view, &[], Profile::Windows).unwrap();
    Raster::new(&mut pixels, &font, geometry, w * 4)
        .unwrap()
        .paint(&scene, geometry.bounds())
        .unwrap();
    pixels
}

#[allow(clippy::unwrap_used, reason = "test reads a known in-frame coordinate")]
fn color(pixels: &[u8], width: usize, x: usize, y: usize) -> u32 {
    let at = (y * width + x) * 4;
    u32::from_le_bytes(pixels.get(at..at + 4).unwrap().try_into().unwrap())
}

fn inside(rect: Rect, x: usize, y: usize) -> bool {
    // A per-pixel i128 reference, independent of production intersection math.
    x as i128 >= i128::from(rect.x)
        && y as i128 >= i128::from(rect.y)
        && (x as i128) < i128::from(rect.x) + i128::from(rect.width)
        && (y as i128) < i128::from(rect.y) + i128::from(rect.height)
}

#[test]
fn fills_match_a_pixel_reference_and_leave_padding_and_tail_untouched() {
    let font = font::pinned().unwrap();
    let geometry = geometry(17, 13, 1);
    let stride = 80;
    let positions = [i64::MIN, -19, -1, 0, 7, 17, i64::MAX];
    for x in positions {
        for y in positions {
            for width in [0, 1, 12, u32::MAX] {
                let rect = Rect {
                    x,
                    y,
                    width,
                    height: width,
                };
                let clip = Rect {
                    x: 2,
                    y: -2,
                    width: 10,
                    height: 12,
                };
                let mut actual = vec![0xaa; stride * 13 + 24];
                Raster::new(&mut actual, &font, geometry, stride)
                    .unwrap()
                    .draw(Draw {
                        clip,
                        primitive: Primitive::Fill {
                            rect,
                            color: 0x00345678,
                        },
                    });
                let mut expected = vec![0xaa; actual.len()];
                for row in 0..13 {
                    for column in 0..17 {
                        if inside(rect, column, row) && inside(clip, column, row) {
                            let at = row * stride + column * 4;
                            expected
                                .get_mut(at..at + 4)
                                .unwrap()
                                .copy_from_slice(&[0x78, 0x56, 0x34, 0xff]);
                        }
                    }
                }
                assert_eq!(actual, expected, "{rect:?}");
            }
        }
    }
}

#[test]
fn glyph_scaling_clipping_and_fallback_match_font_row_bits() {
    let font = font::pinned().unwrap();
    let clip = Rect {
        x: 1,
        y: 2,
        width: 42,
        height: 30,
    };
    for scale in 1..=4 {
        for scalar in ['A', ' ', 'λ', '漢', '\u{10ffff}'] {
            for x in [i64::MIN, -5, 0, 31, i64::MAX] {
                for y in [-3, 0, 28] {
                    let geometry = geometry(49, 35, scale);
                    let mut actual = vec![0xaa; 49 * 35 * 4];
                    Raster::new(&mut actual, &font, geometry, 49 * 4)
                        .unwrap()
                        .draw(Draw {
                            clip,
                            primitive: Primitive::Glyph {
                                x,
                                y,
                                scalar,
                                color: 0xabcdef,
                            },
                        });
                    let mut expected = vec![0xaa; actual.len()];
                    for row in 0..35 {
                        for col in 0..49 {
                            let dx = col as i128 - i128::from(x);
                            let dy = row as i128 - i128::from(y);
                            if !inside(clip, col, row)
                                || dx < 0
                                || dy < 0
                                || dx >= 8 * i128::from(scale)
                                || dy >= 16 * i128::from(scale)
                            {
                                continue;
                            }
                            let bits = font
                                .row(font.index(scalar), dy as usize / usize::from(scale))
                                .unwrap();
                            let set = bits.first().unwrap()
                                & (0x80 >> (dx as usize / usize::from(scale)))
                                != 0;
                            if set {
                                let at = (row * 49 + col) * 4;
                                expected
                                    .get_mut(at..at + 4)
                                    .unwrap()
                                    .copy_from_slice(&[0xef, 0xcd, 0xab, 0xff]);
                            }
                        }
                    }
                    assert_eq!(actual, expected, "{scalar}, scale {scale}, ({x},{y})");
                }
            }
        }
    }
}

#[test]
fn frame_scale_stride_font_and_size_errors_precede_all_writes() {
    for scale in [0, 5, 255] {
        assert_eq!(Scale::new(scale), Err(Error::InvalidArgument));
    }
    for (w, h) in [(0, 1), (1, 0), (8193, 1), (1, 8193), (usize::MAX, 1)] {
        assert_eq!(
            Geometry::new(w, h, Scale::new(1).unwrap()),
            Err(Error::InvalidArgument)
        );
    }
    assert_eq!(
        Geometry::new(8192, 8192, Scale::new(1).unwrap()),
        Err(Error::Limit)
    );
    assert!(Geometry::new(4096, 2048, Scale::new(4).unwrap()).is_ok());
    let font = font::pinned().unwrap();
    let geometry = geometry(4, 4, 1);
    let mut data = vec![0xaa; 64];
    for stride in [0, 15, 17, 20, usize::MAX - 3] {
        assert!(Raster::new(&mut data, &font, geometry, stride).is_err());
        assert!(data.iter().all(|b| *b == 0xaa));
    }
    let mut face = vec![0x72, 0xb5, 0x4a, 0x86];
    for word in [0u32, 32, 1, 1, 1, 1, 1] {
        face.extend_from_slice(&word.to_le_bytes());
    }
    face.extend_from_slice(b"\0 \xff");
    let wrong_font = Font::parse(&face).unwrap();
    assert!(Raster::new(&mut data, &wrong_font, geometry, 16).is_err());
    assert!(data.iter().all(|b| *b == 0xaa));
}

#[test]
fn status_columns_reset_after_newlines_and_expand_tabs() {
    let source = "a long first line\t\nab\tc\n\tz";
    let editor = editor(
        source,
        Selection {
            anchor: source.len(),
            caret: source.len(),
        },
    );
    let geometry = geometry(800, 600, 1);
    let scene = Scene::new(&editor, geometry, View::default(), &[], Profile::Windows).unwrap();
    let mut status = String::new();
    scene.emit(geometry.bounds(), &mut |draw| {
        if let Primitive::Glyph { y: 580, scalar, .. } = draw.primitive {
            status.push(scalar);
        }
    });
    assert!(status.starts_with("Ln 3, Col 10   LF"), "{status}");
}

#[test]
fn newline_selection_requires_a_full_visible_cell() {
    for scale in 1..=4 {
        let s = usize::from(scale);
        let width = 48 * s + 8 * s - 1;
        let geometry = geometry(width, 96 * s, scale);
        for (source, left, soft_wrap) in [("abcd\n", 0, true), ("abcdefghi\n", 5, false)] {
            let editor = editor(
                source,
                Selection {
                    anchor: source.len() - 1,
                    caret: source.len(),
                },
            );
            let data = pixels(
                &editor,
                geometry,
                View {
                    origin: Position {
                        row: 0,
                        column: left,
                    },
                    soft_wrap,
                    caret_visible: false,
                    ..View::default()
                },
            );
            for y in 48 * s..64 * s {
                for x in 40 * s..width - 8 * s {
                    assert_eq!(color(&data, width, x, y), 0xff000000 | render::PAPER);
                }
            }
        }
    }
}

#[test]
fn tabs_and_logical_newlines_have_exact_selection_backgrounds() {
    let editor = editor(
        "a\t\n\n漢z",
        Selection {
            anchor: 1,
            caret: 4,
        },
    );
    let geometry = geometry(96, 136, 1);
    let view = View {
        caret_visible: false,
        ..View::default()
    };
    let focused = pixels(&editor, geometry, view);
    for (x, y) in [(17, 49), (70, 49), (73, 49), (9, 65)] {
        assert_eq!(color(&focused, 96, x, y), 0xff000000 | render::SELECTED);
    }
    assert_eq!(color(&focused, 96, 81, 49), 0xff000000 | render::PAPER);
    let inactive = pixels(
        &editor,
        geometry,
        View {
            focused: false,
            ..view
        },
    );
    assert_eq!(
        color(&inactive, 96, 17, 49),
        0xff000000 | render::INACTIVE_SELECTION
    );
    let doc = editor.document(editor.active().unwrap()).unwrap();
    assert_eq!(doc.text(), "a\t\n\n漢z");
    assert_eq!(
        doc.selection(),
        Selection {
            anchor: 1,
            caret: 4
        }
    );
    assert!(!doc.dirty());
    assert_eq!(doc.revision(), 0);
}

#[test]
fn wrapping_scrolling_and_partial_tabs_use_layout_cell_intervals() {
    let editor = editor(
        "abcd ef",
        Selection {
            anchor: 0,
            caret: 0,
        },
    );
    let geometry = geometry(48, 136, 1);
    let view = View {
        origin: Position {
            row: 1,
            column: 999,
        },
        caret_visible: false,
        ..View::default()
    };
    let scene = Scene::new(&editor, geometry, view, &[], Profile::Windows).unwrap();
    let mut visible = String::new();
    scene.emit(geometry.document(), &mut |draw| {
        if let Primitive::Glyph { scalar, .. } = draw.primitive {
            visible.push(scalar);
        }
    });
    assert_eq!(visible, " ef");
    let editor = self::editor(
        "\tAz\nend",
        Selection {
            anchor: 0,
            caret: 1,
        },
    );
    let actual = pixels(
        &editor,
        geometry,
        View {
            soft_wrap: false,
            origin: Position { row: 0, column: 3 },
            caret_visible: false,
            ..View::default()
        },
    );
    // The tab's left endpoint is offscreen; its remaining span is selected.
    assert_eq!(color(&actual, 48, 8, 48), 0xff000000 | render::SELECTED);
    assert_eq!(color(&actual, 48, 39, 48), 0xff000000 | render::SELECTED);
}

#[test]
fn caret_affinity_and_oversized_tabs_stay_visible_at_each_scale() {
    for scale in 1..=4 {
        let s = usize::from(scale);
        let geometry = geometry(32 * s, 120 * s, scale);
        let editor = editor(
            "abx",
            Selection {
                anchor: 2,
                caret: 2,
            },
        );
        for (affinity, x, y) in [(Affinity::Upstream, 23, 48), (Affinity::Downstream, 8, 64)] {
            let actual = pixels(
                &editor,
                geometry,
                View {
                    affinity,
                    ..View::default()
                },
            );
            for offset in 0..s {
                assert_eq!(
                    color(&actual, 32 * s, x * s + offset, y * s),
                    0xff000000 | render::INK
                );
            }
        }
        let editor = self::editor(
            "\t",
            Selection {
                anchor: 1,
                caret: 1,
            },
        );
        let actual = pixels(
            &editor,
            self::geometry(24 * s, 120 * s, scale),
            View::default(),
        );
        assert_eq!(
            color(&actual, 24 * s, 15 * s, 48 * s),
            0xff000000 | render::INK
        );
    }
}

#[test]
fn damage_repainting_is_identical_to_a_full_frame() {
    let editor = editor(
        "ab\tcd\nλ",
        Selection {
            anchor: 1,
            caret: 6,
        },
    );
    let geometry = geometry(177, 141, 1);
    let full = pixels(&editor, geometry, View::default());
    let font = font::pinned().unwrap();
    let scene = Scene::new(&editor, geometry, View::default(), &[], Profile::Windows).unwrap();
    let mut damaged = vec![0xaa; full.len()];
    let mut raster = Raster::new(&mut damaged, &font, geometry, 177 * 4).unwrap();
    for rect in [
        Rect {
            x: 0,
            y: 0,
            width: 83,
            height: 70,
        },
        Rect {
            x: 83,
            y: 0,
            width: 94,
            height: 70,
        },
        Rect {
            x: 0,
            y: 70,
            width: 177,
            height: 71,
        },
    ] {
        raster.paint(&scene, rect).unwrap();
    }
    assert_eq!(damaged, full);
    let before = damaged.clone();
    let wrong = Scene::new(
        &editor,
        self::geometry(177, 141, 2),
        View::default(),
        &[],
        Profile::Windows,
    )
    .unwrap();
    assert_eq!(
        Raster::new(&mut damaged, &font, geometry, 177 * 4)
            .unwrap()
            .paint(&wrong, geometry.bounds()),
        Err(Error::InvalidArgument)
    );
    assert_eq!(damaged, before);
}

#[test]
fn extreme_resizes_and_empty_sessions_only_touch_visible_frame_bytes() {
    let font = font::pinned().unwrap();
    let editor = Editor::default();
    for scale in 1..=4 {
        for (w, h) in [
            (1, 1),
            (15, 20),
            (16, 72),
            (17, 73),
            (8192, 1),
            (1, 8192),
            (800, 600),
        ] {
            let geometry = geometry(w, h, scale);
            let stride = (w + 2) * 4;
            let mut data = vec![0xaa; stride * h + 16];
            let scene =
                Scene::new(&editor, geometry, View::default(), &[], Profile::Windows).unwrap();
            Raster::new(&mut data, &font, geometry, stride)
                .unwrap()
                .paint(&scene, geometry.bounds())
                .unwrap();
            for row in data.get(..stride * h).unwrap().chunks_exact(stride) {
                assert!(row.get(w * 4..).unwrap().iter().all(|b| *b == 0xaa));
                assert!(row
                    .get(..w * 4)
                    .unwrap()
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|pixel| pixel.last() == Some(&0xff)));
            }
            assert!(data.get(stride * h..).unwrap().iter().all(|b| *b == 0xaa));
        }
    }
}

#[test]
fn tab_geometry_keeps_the_active_tab_visible_and_labels_are_bounded() {
    let mut editor = Editor::default();
    for _ in 0..64 {
        editor.new_tab().unwrap();
    }
    let geometry = geometry(320, 120, 1);
    assert!(geometry.tab(61, 63, 64).is_none());
    assert_eq!(geometry.tab(62, 63, 64).unwrap().x, 0);
    assert_eq!(geometry.tab(63, 63, 64).unwrap().x, 160);
    assert!(geometry.tab(64, 63, 64).is_none());
    assert!(geometry.tab(0, 0, 65).is_none());
    let tiny = self::geometry(10, 10, 1);
    assert!(tiny.tab(63, 63, 64).is_some());
    let tab = editor.active().unwrap();
    let bad_labels = [Label { tab, title: "a" }, Label { tab, title: "b" }];
    assert!(matches!(
        Scene::new(
            &editor,
            geometry,
            View::default(),
            &bad_labels,
            Profile::Windows
        ),
        Err(Error::InvalidArgument)
    ));
    let too_long = "x".repeat(4097);
    let bad_labels = [Label {
        tab,
        title: &too_long,
    }];
    assert!(matches!(
        Scene::new(
            &editor,
            geometry,
            View::default(),
            &bad_labels,
            Profile::Windows
        ),
        Err(Error::Limit)
    ));
    assert!(matches!(
        Scene::new(
            &editor,
            geometry,
            View {
                origin: Position {
                    row: usize::MAX,
                    column: 0
                },
                ..View::default()
            },
            &[],
            Profile::Windows
        ),
        Err(Error::Limit)
    ));
    let labels = [Label {
        tab,
        title: "a\nb\tc",
    }];
    let too_many: Vec<_> = (0..65).map(|_| Label { tab, title: "x" }).collect();
    assert!(matches!(
        Scene::new(
            &editor,
            geometry,
            View::default(),
            &too_many,
            Profile::Windows
        ),
        Err(Error::Limit)
    ));
    assert!(matches!(
        Scene::new(
            &editor,
            geometry,
            View {
                origin: Position {
                    row: 0,
                    column: td_editor::text::MAX_FILE_BYTES * 8 + 1
                },
                ..View::default()
            },
            &[],
            Profile::Windows,
        ),
        Err(Error::Limit)
    ));
    let scene = Scene::new(
        &editor,
        geometry,
        View::default(),
        &labels,
        Profile::Windows,
    )
    .unwrap();
    let mut replacements = 0;
    scene.emit(geometry.bounds(), &mut |draw| {
        if matches!(
            draw.primitive,
            Primitive::Glyph {
                scalar: '\u{fffd}',
                ..
            }
        ) {
            replacements += 1;
        }
    });
    assert_eq!(replacements, 2);
}

#[test]
fn a_long_unwrapped_document_does_not_emit_offscreen_glyph_operations() {
    let editor = editor(
        &"x".repeat(td_editor::text::MAX_FILE_BYTES),
        Selection {
            anchor: 0,
            caret: 0,
        },
    );
    let geometry = geometry(800, 600, 1);
    let scene = Scene::new(
        &editor,
        geometry,
        View {
            soft_wrap: false,
            caret_visible: false,
            ..View::default()
        },
        &[],
        Profile::Windows,
    )
    .unwrap();
    let mut count = 0;
    scene.emit(geometry.document(), &mut |_| count += 1);
    assert!(count < 110, "emitted {count} draws for 98 visible cells");
}

#[test]
fn the_real_binary_exposes_a_deterministic_preview_and_its_font_notices() {
    let exe = env!("CARGO_BIN_EXE_td-editor");
    let output = std::process::Command::new(exe)
        .arg("--preview")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(output.stdout.starts_with(b"P6\n800 600\n255\n"));
    assert_eq!(output.stdout.len(), 15 + 800 * 600 * 3);
    let mut expected = Vec::new();
    render::preview(&mut expected).unwrap();
    assert_eq!(output.stdout, expected);
    let hash = output
        .stdout
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    assert_eq!(hash, 0xd6c480eb75800984, "preview checksum: {hash:016x}");
    let output = std::process::Command::new(exe)
        .arg("--font-license")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let notices = String::from_utf8(output.stdout).unwrap();
    assert!(notices.contains("SIL OPEN FONT LICENSE Version 1.1"));
    assert!(notices.contains("64019ab811067e03a8de5990d2e6f23dcec5418e5a90caa5e5666b0524156732"));
}
