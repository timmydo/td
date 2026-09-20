#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! Pixel oracles over the raster primitives, independent of any scene:
//! fills and glyphs against per-pixel references, and the validation that
//! precedes every write. Moved intact from td-editor's render suite.

use td_ui::font::{self, Font};
use td_ui::raster::{
    hint_run, text_run, Composition, Draw, Error, GlyphStyle, Primitive, Raster, Rect, Scale,
    Scrollbar, Surface, Weight, INK, PAPER,
};

fn surface(width: usize, height: usize, scale: u8) -> Surface {
    Surface::new(width, height, Scale::new(scale).unwrap()).unwrap()
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
    let surface = surface(17, 13, 1);
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
                Raster::new(&mut actual, &font, surface, stride)
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
        for weight in [Weight::Regular, Weight::Medium] {
            for scalar in ['A', ' ', 'λ', '漢', '█', '\u{10ffff}'] {
                for x in [i64::MIN, -5, 0, 31, i64::MAX] {
                    for y in [-3, 0, 28] {
                        let surface = surface(49, 35, scale);
                        let mut actual = vec![0xaa; 49 * 35 * 4];
                        Raster::new(&mut actual, &font, surface, 49 * 4)
                            .unwrap()
                            .draw(Draw {
                                clip,
                                primitive: Primitive::Glyph {
                                    x,
                                    y,
                                    scalar,
                                    style: GlyphStyle {
                                        ink: 0xabcdef,
                                        // Blue is 413/3: the oracle distinguishes floor from rounding.
                                        background: 0x123457,
                                        weight,
                                    },
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
                                let column = dx as usize / usize::from(scale);
                                let fringe = weight == Weight::Medium
                                    && column > 0
                                    && bits.first().unwrap() & (0x80 >> (column - 1)) != 0;
                                if set || fringe {
                                    let at = (row * 49 + col) * 4;
                                    expected
                                        .get_mut(at..at + 4)
                                        .unwrap()
                                        .copy_from_slice(if set {
                                            &[0xef, 0xcd, 0xab, 0xff]
                                        } else {
                                            &[0x89, 0x67, 0x45, 0xff]
                                        });
                                }
                            }
                        }
                        assert_eq!(
                            actual, expected,
                            "{scalar}, scale {scale}, {weight:?}, ({x},{y})"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn surface_scale_stride_font_and_size_errors_precede_all_writes() {
    for scale in [0, 5, 255] {
        assert_eq!(Scale::new(scale), Err(Error::InvalidArgument));
    }
    assert_eq!(Scale::default(), Scale::new(1).unwrap());
    for (w, h) in [(0, 1), (1, 0), (8193, 1), (1, 8193), (usize::MAX, 1)] {
        assert_eq!(
            Surface::new(w, h, Scale::new(1).unwrap()),
            Err(Error::InvalidArgument)
        );
    }
    assert_eq!(
        Surface::new(8192, 8192, Scale::new(1).unwrap()),
        Err(Error::Limit)
    );
    assert!(Surface::new(4096, 2048, Scale::new(4).unwrap()).is_ok());
    let font = font::pinned().unwrap();
    let surface = surface(4, 4, 1);
    assert_eq!((surface.width, surface.height), (4, 4));
    assert_eq!(
        surface.bounds(),
        Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4
        }
    );
    let mut data = vec![0xaa; 64];
    for stride in [0, 15, 17, 20, usize::MAX - 3] {
        assert!(Raster::new(&mut data, &font, surface, stride).is_err());
        assert!(data.iter().all(|b| *b == 0xaa));
    }
    // A literal surface is held to the same ceilings as a constructed one.
    for (width, height, error) in [
        (0, 4, Error::InvalidArgument),
        (8193, 1, Error::InvalidArgument),
        (8192, 8192, Error::Limit),
    ] {
        let literal = Surface {
            width,
            height,
            scale: Scale::default(),
        };
        assert_eq!(literal.check(), Err(error));
        assert_eq!(
            Raster::new(&mut data, &font, literal, 8192 * 4).map(|_| ()),
            Err(error)
        );
        assert!(data.iter().all(|b| *b == 0xaa));
    }
    let mut face = vec![0x72, 0xb5, 0x4a, 0x86];
    for word in [0u32, 32, 1, 1, 1, 1, 1] {
        face.extend_from_slice(&word.to_le_bytes());
    }
    face.extend_from_slice(b"\0 \xff");
    let wrong_font = Font::parse(&face).unwrap();
    assert!(Raster::new(&mut data, &wrong_font, surface, 16).is_err());
    assert!(data.iter().all(|b| *b == 0xaa));
}

#[test]
fn rgb_rows_skip_stride_padding_and_validation_precedes_the_copy() {
    use td_ui::raster::{ppm, rgb, Error, Scale, Surface};
    let s = surface(3, 2, 1);
    // Two rows at a 16-byte stride: three XRGB pixels, four padding bytes.
    let mut pixels = vec![0xaa; 32];
    for (i, px) in [[1u8, 2, 3, 0], [4, 5, 6, 0], [7, 8, 9, 0]]
        .iter()
        .enumerate()
    {
        pixels[i * 4..i * 4 + 4].copy_from_slice(px);
    }
    for (i, px) in [[10u8, 11, 12, 0], [13, 14, 15, 0], [16, 17, 18, 0]]
        .iter()
        .enumerate()
    {
        pixels[16 + i * 4..16 + i * 4 + 4].copy_from_slice(px);
    }
    let rows = rgb(&pixels, s, 16).unwrap();
    assert_eq!(
        rows,
        [3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10, 15, 14, 13, 18, 17, 16]
    );
    assert_eq!(
        ppm(s, &rows),
        [b"P6\n3 2\n255\n".as_slice(), &rows].concat()
    );
    // Validation mirrors `Raster::new`, before any byte is copied.
    assert_eq!(rgb(&pixels, s, 8), Err(Error::InvalidArgument));
    assert_eq!(rgb(&pixels, s, 14), Err(Error::InvalidArgument));
    assert_eq!(rgb(&pixels[..31], s, 16), Err(Error::InvalidArgument));
    let empty = Surface {
        width: 0,
        height: 2,
        scale: Scale::new(1).unwrap(),
    };
    assert_eq!(rgb(&pixels, empty, 16), Err(Error::InvalidArgument));
    assert_eq!(rgb(&pixels, s, usize::MAX & !3), Err(Error::Limit));
}

/// A composition laid out for another surface is refused before a write.
#[test]
fn a_composition_for_another_surface_is_refused_unpainted() {
    struct Fill(Surface);
    impl Composition for Fill {
        fn surface(&self) -> Surface {
            self.0
        }
        fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
            sink(Draw {
                clip: damage,
                primitive: Primitive::Fill {
                    rect: self.0.bounds(),
                    color: 0x123456,
                },
            });
        }
    }
    let font = font::pinned().unwrap();
    let mut data = vec![0xaa; 4 * 4 * 4];
    let mut raster = Raster::new(&mut data, &font, surface(4, 4, 1), 16).unwrap();
    assert_eq!(
        raster.paint(&Fill(surface(4, 4, 2)), surface(4, 4, 1).bounds()),
        Err(Error::InvalidArgument)
    );
    assert!(raster
        .paint(&Fill(surface(4, 4, 1)), surface(4, 4, 1).bounds())
        .is_ok());
    // A composition behind a trait object paints too.
    let dynamic: &dyn Composition = &Fill(surface(4, 4, 1));
    assert!(raster.paint(dynamic, surface(4, 4, 1).bounds()).is_ok());
    assert!(data.chunks(4).all(|px| px == [0x56, 0x34, 0x12, 0xff]));
}

/// The text-run painter takes one cell per scalar from the origin, stops
/// at the bounds' right edge, substitutes U+FFFD for a control scalar and
/// emits nothing outside the damage.
#[test]
fn text_runs_fill_whole_cells_inside_bounds_and_damage() {
    let bounds = Rect {
        x: 8,
        y: 0,
        width: 40,
        height: 16,
    };
    let damage = Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    };
    let style = GlyphStyle::medium(INK, PAPER);
    for scale in 1..=4u8 {
        let scale = Scale::new(scale).unwrap();
        let cw = 8 * scale.value() as i64;
        let mut draws = Vec::new();
        text_run(
            scale,
            "ab\tcdefgh".chars(),
            (12, 3),
            bounds,
            style,
            damage,
            &mut |draw| draws.push(draw),
        );
        // Whole cells between the origin and the right edge: 36 pixels.
        assert_eq!(draws.len(), (36 / cw) as usize, "scale {}", scale.value());
        for (index, draw) in draws.iter().enumerate() {
            assert_eq!(
                draw.clip, bounds,
                "bounds inside the damage clip to themselves"
            );
            let Primitive::Glyph {
                x,
                y,
                scalar,
                style: painted,
            } = draw.primitive
            else {
                panic!("a text run is glyphs");
            };
            assert_eq!((x, y), (12 + index as i64 * cw, 3));
            assert_eq!(painted, style);
            assert_eq!(scalar, "ab\u{fffd}cdefgh".chars().nth(index).unwrap());
        }
        let mut count = 0;
        let far = Rect {
            x: 500,
            y: 500,
            width: 1,
            height: 1,
        };
        text_run(scale, "x".chars(), (12, 3), bounds, style, far, &mut |_| {
            count += 1
        });
        assert_eq!(count, 0, "no draw outside the damage");
        text_run(
            scale,
            "x".chars(),
            (48, 0),
            bounds,
            style,
            damage,
            &mut |_| count += 1,
        );
        assert_eq!(count, 0, "no draw past the right edge");
        // A run at the far end of the coordinate space neither wraps nor
        // panics: the slot count bounds every origin it emits.
        let edge = Rect {
            x: i64::MAX - 8,
            y: 0,
            width: u32::MAX,
            height: 16,
        };
        let mut origins = Vec::new();
        text_run(
            scale,
            "abc".chars(),
            (i64::MAX - 8, 0),
            edge,
            style,
            edge,
            &mut |draw| {
                if let Primitive::Glyph { x, .. } = draw.primitive {
                    origins.push(x);
                }
            },
        );
        assert_eq!(
            origins,
            if scale.value() == 1 {
                vec![i64::MAX - 8]
            } else {
                vec![]
            }
        );
    }
}

/// A hint run lays as many marks as fit whole from its origin to the
/// bounds' right, `ADVANCE` apart, the last needing no space after it,
/// each a `Mark` in the ink; nothing outside the damage or past the
/// right edge, and no wrap at the far end of the coordinate space. The
/// marks paint the face's rows at the scale, bit 3 the leftmost column,
/// and nothing where a row is unlit.
#[test]
fn hint_runs_fit_whole_marks_and_paint_the_face_at_the_scale() {
    use td_ui::hint::{self, ADVANCE, WIDTH};
    let damage = Rect {
        x: 0,
        y: 0,
        width: 200,
        height: 200,
    };
    for scale in 1..=4u8 {
        let s = scale as i64;
        let scale = Scale::new(scale).unwrap();
        let count = |width: u32| {
            let mut out = Vec::new();
            let bounds = Rect {
                x: 10,
                y: 0,
                width,
                height: 40,
            };
            hint_run(
                scale,
                "F12".chars(),
                (10, 5),
                bounds,
                INK,
                damage,
                &mut |draw| {
                    let Primitive::Mark { x, y, scalar, ink } = draw.primitive else {
                        panic!("a hint run is marks");
                    };
                    assert_eq!((draw.clip, y, ink), (bounds, 5, INK));
                    out.push((x, scalar));
                },
            );
            out
        };
        // Exactly the text's width holds it all; a pixel less loses the
        // last mark; one mark's width holds one.
        let full = count((hint::width("F12") * scale.value()) as u32);
        assert_eq!(
            full,
            vec![
                (10, 'F'),
                (10 + ADVANCE as i64 * s, '1'),
                (10 + 2 * ADVANCE as i64 * s, '2')
            ]
        );
        assert_eq!(
            count((hint::width("F12") * scale.value()) as u32 - 1).len(),
            2
        );
        assert_eq!(count((WIDTH * scale.value()) as u32).len(), 1);
        assert_eq!(count((WIDTH * scale.value()) as u32 - 1).len(), 0);
        let far = Rect {
            x: 500,
            y: 500,
            width: 1,
            height: 1,
        };
        let mut n = 0;
        let bounds = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        hint_run(scale, "x".chars(), (10, 5), bounds, INK, far, &mut |_| {
            n += 1
        });
        hint_run(
            scale,
            "x".chars(),
            (100, 5),
            bounds,
            INK,
            damage,
            &mut |_| n += 1,
        );
        assert_eq!(n, 0);
        let edge = Rect {
            x: i64::MAX - 4,
            y: 0,
            width: u32::MAX,
            height: 16,
        };
        let mut origins = Vec::new();
        hint_run(
            scale,
            "abc".chars(),
            (i64::MAX - 4, 0),
            edge,
            INK,
            edge,
            &mut |draw| {
                if let Primitive::Mark { x, .. } = draw.primitive {
                    origins.push(x);
                }
            },
        );
        assert_eq!(
            origins,
            if scale.value() == 1 {
                vec![i64::MAX - 4]
            } else {
                vec![]
            }
        );
        // The pixels of a mark: '1' is `.#..` over `##..`, `.#..`, `.#..`,
        // `###.`; the unlit cells keep what was under them.
        let surface = surface(32, 32, scale.value() as u8);
        let font = font::pinned().unwrap();
        let mut pixels = vec![0x11; 32 * 32 * 4 * scale.value() * scale.value()];
        let mut raster = Raster::new(&mut pixels, &font, surface, surface.width * 4).unwrap();
        raster.draw(Draw {
            clip: surface.bounds(),
            primitive: Primitive::Mark {
                x: 3,
                y: 2,
                scalar: '1',
                ink: INK,
            },
        });
        let pixel = |x: i64, y: i64| {
            let offset = (y as usize * surface.width + x as usize) * 4;
            u32::from_le_bytes([
                pixels[offset],
                pixels[offset + 1],
                pixels[offset + 2],
                pixels[offset + 3],
            ]) & 0xffffff
        };
        assert_eq!(pixel(3, 2), 0x111111);
        assert_eq!(pixel(3 + s, 2), INK);
        assert_eq!(pixel(3 + 2 * s - 1, 2 + s - 1), INK);
        assert_eq!(pixel(3 + 2 * s, 2), 0x111111);
        assert_eq!(pixel(3, 2 + s), INK);
        assert_eq!(pixel(3, 2 + 4 * s), INK);
        assert_eq!(pixel(3 + 2 * s, 2 + 4 * s), INK);
        assert_eq!(pixel(3 + 3 * s, 2 + 4 * s), 0x111111);
        assert_eq!(pixel(3, 2 + 5 * s), 0x111111);
    }
}

/// The scrollbar's thumb is proportional, at least 24 pixels per unit of
/// scale, and leaves `scale` pixels of travel whenever scrolling is
/// possible; dragging maps pixels back to units anchored at the grab
/// origin. Zero counts and tracks shorter than a cell, which no editor
/// viewport produces, stay inside the track without a division or an
/// underflow.
#[test]
fn scrollbar_thumb_and_travel_are_bounded_along_either_axis() {
    for scale in 1..=4u8 {
        let s = Scale::new(scale).unwrap();
        let scale = usize::from(scale);
        let track = Rect {
            x: 100,
            y: 20,
            width: (12 * scale) as u32,
            height: (400 * scale) as u32,
        };
        let full = Scrollbar::new(track, 40, 40, 0, s, false);
        assert!(!full.enabled());
        assert_eq!(full.thumb, track);
        let bar = Scrollbar::new(track, 10, 100, 0, s, false);
        assert!(bar.enabled() && !bar.horizontal());
        assert_eq!(bar.thumb.height, (40 * scale) as u32);
        assert_eq!(bar.thumb.y, track.y);
        let last = Scrollbar::new(track, 10, 100, 90, s, false);
        assert_eq!(
            last.thumb.y + i64::from(last.thumb.height),
            track.y + i64::from(track.height)
        );
        let tiny = Scrollbar::new(track, 1, 100_000, 0, s, false);
        assert_eq!(tiny.thumb.height, (24 * scale) as u32);
        assert_eq!(bar.position_at(bar.thumb.y, 0, 0), 0);
        assert_eq!(bar.position_at(track.y + i64::from(track.height), 0, 0), 90);
        assert_eq!(
            bar.position_at(bar.thumb.y, 0, 7),
            7,
            "no drift at the grab origin"
        );
        let wide = Rect {
            x: 0,
            y: 0,
            width: (400 * scale) as u32,
            height: (12 * scale) as u32,
        };
        let row = Scrollbar::new(wide, 10, 100, 50, s, true);
        assert!(row.horizontal());
        assert_eq!(row.coordinate(3, 4), 3);
        assert_eq!(row.thumb.width, (40 * scale) as u32);
        assert_eq!(row.position_at(row.thumb.x, 0, 50), 50);
        // The fields are public: a thumb edited past its track is no
        // underflow, and the position stays clamped.
        let mut edited = bar;
        edited.thumb.height = track.height + 5;
        assert_eq!(edited.position_at(edited.thumb.y, 0, 7), 7);
        assert!(edited.position_at(track.y + i64::from(track.height), 0, 0) <= 90);
        for (visible, total) in [(0, 0), (0, 5), (5, 0)] {
            let empty = Scrollbar::new(track, visible, total, 3, s, false);
            assert_eq!(empty.enabled(), total > visible, "{visible}/{total}");
            assert_eq!(empty.thumb.intersection(track), Some(empty.thumb));
            assert!(empty.position_at(empty.thumb.y, 0, 0) <= total.saturating_sub(visible));
        }
        for length in 1..(8 * scale) as u32 {
            let short = Rect {
                x: 0,
                y: 0,
                width: 1,
                height: length,
            };
            let stub = Scrollbar::new(short, 1, 100, 50, s, false);
            assert_eq!(stub.thumb.height, length.saturating_sub(scale as u32));
            assert!(stub.thumb.y >= 0);
            assert!(stub.thumb.y + i64::from(stub.thumb.height) <= i64::from(length));
            for at in [-1, 0, 1, i64::from(length)] {
                assert!(stub.position_at(at, 0, 50) <= 99);
            }
        }
    }
}
