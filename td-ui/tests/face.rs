#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The outline face and the raster's execution of glyphs through it, over
//! fonts `fonts` encodes: the cell from the face's metrics, the face's
//! refusals, atlas reuse and misses, and pixel oracles for the blend over
//! the explicit background (never the buffer), the bitmap fallback
//! centred in the cell, blank glyphs, the cell clip and the bold style.

mod fonts;

use fonts::{Builder, Glyph, Segment};
use td_ui::atlas::{Slot, Style};
use td_ui::face::{Cell, Face};
use td_ui::font::pinned;
use td_ui::raster::{Draw, GlyphStyle, Primitive, Raster, Rect, Scale, Surface, Weight};
use td_ui::sfnt::Error;

const GARBAGE: u8 = 0x5a;
const INK: u32 = 0x102030;
const PAPER: u32 = 0xf0e0d0;

fn rectangle(left: i32, bottom: i32, right: i32, top: i32) -> Vec<(i32, i32, bool)> {
    vec![
        (left, bottom, true),
        (left, top, true),
        (right, top, true),
        (right, bottom, true),
    ]
}

/// '0' and 'a' a 500-unit square on the baseline, ' ' blank, 'T' twice
/// the cell wide and 'h' a square half a pixel in at 20 px/em; `small`
/// makes 'a' a quarter of the size, for a bold face that differs.
fn face_bytes(small: bool) -> Vec<u8> {
    let a = if small {
        rectangle(0, 0, 250, 250)
    } else {
        rectangle(0, 0, 500, 500)
    };
    let mut builder = Builder::new(vec![
        Glyph::Empty,
        Glyph::Simple(vec![rectangle(0, 0, 500, 500)]),
        Glyph::Simple(vec![a]),
        Glyph::Empty,
        Glyph::Simple(vec![rectangle(0, -200, 1000, 800)]),
        Glyph::Simple(vec![rectangle(25, 0, 525, 500)]),
    ]);
    let map = |scalar: char, glyph: u16| {
        let code = u32::from(scalar) as u16;
        Segment::Delta(code, code, glyph.wrapping_sub(code))
    };
    builder.format4 = vec![
        map(' ', 3),
        map('0', 1),
        map('T', 4),
        map('a', 2),
        map('h', 5),
    ];
    builder.font()
}

fn surface(width: usize, height: usize, scale: u8) -> Surface {
    Surface::new(width, height, Scale::new(scale).unwrap()).unwrap()
}

fn style(weight: Weight) -> GlyphStyle {
    GlyphStyle {
        ink: INK,
        background: PAPER,
        weight,
    }
}

fn glyph(x: i64, y: i64, scalar: char, weight: Weight) -> Draw {
    Draw {
        clip: Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        },
        primitive: Primitive::Glyph {
            x,
            y,
            scalar,
            style: style(weight),
        },
    }
}

fn paint(face: &mut Face, surface: Surface, draws: &[Draw]) -> Vec<u8> {
    let font = pinned().unwrap();
    let mut pixels = vec![GARBAGE; surface.width * surface.height * 4];
    let mut raster = Raster::new(&mut pixels, &font, surface, surface.width * 4)
        .unwrap()
        .with_face(face);
    for draw in draws {
        raster.draw(*draw);
    }
    pixels
}

fn pixel(pixels: &[u8], surface: Surface, x: usize, y: usize) -> u32 {
    let at = (y * surface.width + x) * 4;
    u32::from_le_bytes(pixels[at..at + 4].try_into().unwrap())
}

fn untouched(pixels: &[u8], surface: Surface, x: usize, y: usize) -> bool {
    pixel(pixels, surface, x, y) == u32::from_le_bytes([GARBAGE; 4])
}

fn mix(alpha: u32) -> u32 {
    let mut color = 0;
    for shift in [0, 8, 16] {
        let from = (PAPER >> shift) & 255;
        let to = (INK >> shift) & 255;
        color |= ((from * (255 - alpha) + to * alpha + 127) / 255) << shift;
    }
    color | 0xff000000
}

#[test]
fn the_cell_comes_from_the_face_metrics() {
    let face = Face::new(face_bytes(false), None, 20).unwrap();
    // Advance 501 of 1000 units, ascender 800, descender -200, gap 90.
    assert_eq!(
        face.cell(),
        Cell {
            width: 10,
            height: 22,
            baseline: 16
        }
    );
    assert_eq!(face.pixels_per_em(), 20);
    let face = Face::new(face_bytes(false), None, 13).unwrap();
    assert_eq!(
        face.cell(),
        Cell {
            width: 7,
            height: 14,
            baseline: 10
        }
    );
}

#[test]
fn faces_are_refused_by_size_metrics_and_bytes() {
    for size in [5, 257] {
        assert_eq!(
            Face::new(face_bytes(false), None, size).unwrap_err(),
            Error::Limit("pixels per em")
        );
    }
    assert!(Face::new(face_bytes(false), None, 6).is_ok());
    assert!(Face::new(face_bytes(false), None, 256).is_ok());
    let mut builder = Builder::new(vec![Glyph::Empty, Glyph::Empty]);
    builder.format4 = vec![Segment::Delta(0x41, 0x41, 1u16.wrapping_sub(0x41))];
    assert_eq!(
        Face::new(builder.font(), None, 20).unwrap_err(),
        Error::Missing("cell advance")
    );
    assert_eq!(
        Face::new(vec![0; 3], None, 20).unwrap_err(),
        Error::Truncated("table directory")
    );
    assert_eq!(
        Face::new(face_bytes(false), Some(b"OTTO\0\0\0\0".to_vec()), 20).unwrap_err(),
        Error::Unsupported("CFF outlines")
    );
}

#[test]
fn glyphs_are_covered_once_and_misses_are_remembered() {
    let mut face = Face::new(face_bytes(false), None, 20).unwrap();
    let Slot::Placed(entry) = face.glyph(Style::Regular, 'a') else {
        panic!("placed");
    };
    assert_eq!(
        (entry.width, entry.height, entry.left, entry.top),
        (10, 10, 0, 10)
    );
    assert!(face.take_dirty().is_some());
    assert_eq!(face.glyph(Style::Regular, 'a'), Slot::Placed(entry));
    assert_eq!(face.take_dirty(), None, "a hit writes nothing");
    assert_eq!(face.glyph(Style::Regular, ' '), Slot::Blank);
    assert_eq!(face.glyph(Style::Regular, 'Z'), Slot::Missing);
    assert_eq!(face.atlas().get(Style::Regular, 'Z'), Some(Slot::Missing));
    assert_eq!((face.atlas().len(), face.atlas().epoch()), (3, 0));
    assert_eq!(face.style(true), Style::Regular, "no bold face");
    assert_eq!(face.style(false), Style::Regular);
    let face = Face::new(face_bytes(false), Some(face_bytes(true)), 20).unwrap();
    assert_eq!(face.style(true), Style::Bold);
}

#[test]
fn glyphs_blend_over_the_explicit_background_and_write_nothing_else() {
    let mut face = Face::new(face_bytes(false), None, 20).unwrap();
    let s = surface(40, 30, 1);
    // The cell's top-left at (5, 3): baseline at row 19.
    let pixels = paint(&mut face, s, &[glyph(5, 3, 'a', Weight::Regular)]);
    for y in 0..30 {
        for x in 0..40 {
            if (5..15).contains(&x) && (9..19).contains(&y) {
                assert_eq!(pixel(&pixels, s, x, y), INK | 0xff000000, "({x}, {y})");
            } else {
                assert!(untouched(&pixels, s, x, y), "({x}, {y})");
            }
        }
    }
    // Half a pixel in: the first column half covered, the eleventh beyond
    // the cell and so clipped.
    let pixels = paint(&mut face, s, &[glyph(5, 3, 'h', Weight::Regular)]);
    for y in 9..19 {
        assert_eq!(pixel(&pixels, s, 5, y), mix(128));
        assert_eq!(pixel(&pixels, s, 6, y), mix(255));
        assert_eq!(pixel(&pixels, s, 14, y), mix(255));
        assert!(untouched(&pixels, s, 15, y));
    }
    assert!(mix(128) != INK | 0xff000000 && mix(128) != PAPER | 0xff000000);
}

#[test]
fn glyphs_stay_inside_their_cell_and_the_draw_clip() {
    let mut face = Face::new(face_bytes(false), None, 20).unwrap();
    let s = surface(40, 30, 1);
    // 'T' is 20 px wide from the pen and reaches 4 px below the baseline.
    let pixels = paint(&mut face, s, &[glyph(5, 3, 'T', Weight::Regular)]);
    for y in 0..30 {
        for x in 0..40 {
            let lit = (5..15).contains(&x) && (3..23).contains(&y);
            assert_eq!(!untouched(&pixels, s, x, y), lit, "({x}, {y})");
        }
    }
    let mut clipped = glyph(5, 3, 'T', Weight::Regular);
    clipped.clip = Rect {
        x: 8,
        y: 10,
        width: 3,
        height: 2,
    };
    let pixels = paint(&mut face, s, &[clipped]);
    let lit = (0..30)
        .flat_map(|y| (0..40).map(move |x| (x, y)))
        .filter(|&(x, y)| !untouched(&pixels, s, x, y))
        .count();
    assert_eq!(lit, 6);
    // Off the surface entirely: nothing, and no fault.
    let pixels = paint(&mut face, s, &[glyph(-500, 3, 'T', Weight::Regular)]);
    assert!(pixels.iter().all(|&b| b == GARBAGE));
}

#[test]
fn blank_glyphs_write_nothing_and_missing_ones_fall_back_centred() {
    let mut face = Face::new(face_bytes(false), None, 20).unwrap();
    let s = surface(40, 30, 1);
    let pixels = paint(&mut face, s, &[glyph(5, 3, ' ', Weight::Regular)]);
    assert!(pixels.iter().all(|&b| b == GARBAGE));

    // 'Z' is not in the face: the bitmap face's glyph, its 8x16 centred in
    // the 10x22 cell, exactly as a bitmap raster paints it there.
    for weight in [Weight::Regular, Weight::Medium] {
        let pixels = paint(&mut face, s, &[glyph(5, 3, 'Z', weight)]);
        let font = pinned().unwrap();
        let mut expected = vec![GARBAGE; 40 * 30 * 4];
        Raster::new(&mut expected, &font, s, 160)
            .unwrap()
            .draw(glyph(6, 6, 'Z', weight));
        assert_eq!(pixels, expected);
        assert!(pixels.iter().any(|&b| b != GARBAGE));
    }

    // At scale 2 the bitmap glyph doubles; the cell is the face's own.
    let mut face = Face::new(face_bytes(false), None, 40).unwrap();
    assert_eq!(
        face.cell(),
        Cell {
            width: 20,
            height: 44,
            baseline: 32
        }
    );
    let s = surface(40, 50, 2);
    let pixels = paint(&mut face, s, &[glyph(0, 0, 'Z', Weight::Regular)]);
    let font = pinned().unwrap();
    let mut expected = vec![GARBAGE; 40 * 50 * 4];
    Raster::new(&mut expected, &font, s, 160)
        .unwrap()
        .draw(glyph(2, 6, 'Z', Weight::Regular));
    assert_eq!(pixels, expected);
}

#[test]
fn medium_weight_draws_the_bold_style_when_the_face_has_one() {
    let s = surface(40, 30, 1);
    let lit = |pixels: &[u8]| {
        (0..30)
            .flat_map(|y| (0..40).map(move |x| (x, y)))
            .filter(|&(x, y)| !untouched(pixels, s, x, y))
            .count()
    };
    let mut plain = Face::new(face_bytes(false), None, 20).unwrap();
    assert_eq!(
        lit(&paint(&mut plain, s, &[glyph(5, 3, 'a', Weight::Medium)])),
        100
    );
    let mut both = Face::new(face_bytes(false), Some(face_bytes(true)), 20).unwrap();
    assert_eq!(
        lit(&paint(&mut both, s, &[glyph(5, 3, 'a', Weight::Medium)])),
        25
    );
    assert_eq!(
        lit(&paint(&mut both, s, &[glyph(5, 3, 'a', Weight::Regular)])),
        100
    );
    assert_eq!(both.atlas().len(), 2, "one slot a style");
}

#[test]
fn other_primitives_are_unchanged_by_a_face() {
    let mut face = Face::new(face_bytes(false), None, 20).unwrap();
    let s = surface(40, 30, 1);
    let fill = Draw {
        clip: s.bounds(),
        primitive: Primitive::Fill {
            rect: Rect {
                x: 2,
                y: 2,
                width: 5,
                height: 5,
            },
            color: 0x123456,
        },
    };
    let mark = Draw {
        clip: s.bounds(),
        primitive: Primitive::Mark {
            x: 20,
            y: 20,
            scalar: 'A',
            ink: 0x654321,
        },
    };
    let pixels = paint(&mut face, s, &[fill, mark]);
    let font = pinned().unwrap();
    let mut expected = vec![GARBAGE; 40 * 30 * 4];
    let mut raster = Raster::new(&mut expected, &font, s, 160).unwrap();
    raster.draw(fill);
    raster.draw(mark);
    assert_eq!(pixels, expected);
    assert!(face.atlas().is_empty());
}

#[test]
fn the_bold_style_has_its_own_scale_and_falls_back_to_regular_outlines() {
    let s = surface(40, 30, 1);
    let lit = |pixels: &[u8]| {
        (0..30)
            .flat_map(|y| (0..40).map(move |x| (x, y)))
            .filter(|&(x, y)| !untouched(pixels, s, x, y))
            .count()
    };
    // A bold face at 2000 units per em whose 'a' is a 1000-unit square:
    // the same 10 pixels at 20 px/em as the regular 500 of 1000. It maps
    // no 'h', which the regular outline then supplies.
    let mut builder = Builder::new(vec![
        Glyph::Empty,
        Glyph::Simple(vec![rectangle(0, 0, 1000, 1000)]),
    ]);
    builder.units_per_em = 2000;
    builder.format4 = vec![
        Segment::Delta(0x30, 0x30, 1u16.wrapping_sub(0x30)),
        Segment::Delta(0x61, 0x61, 1u16.wrapping_sub(0x61)),
    ];
    let mut face = Face::new(face_bytes(false), Some(builder.font()), 20).unwrap();
    assert_eq!(
        lit(&paint(&mut face, s, &[glyph(5, 3, 'a', Weight::Medium)])),
        100
    );
    assert_eq!(
        paint(&mut face, s, &[glyph(5, 3, 'h', Weight::Medium)]),
        paint(&mut face, s, &[glyph(5, 3, 'h', Weight::Regular)])
    );
    assert!(matches!(
        face.atlas().get(Style::Bold, 'h'),
        Some(Slot::Placed(_))
    ));

    // Without a bold face a Bold lookup is the regular slot, not a copy.
    let mut plain = Face::new(face_bytes(false), None, 20).unwrap();
    let regular = plain.glyph(Style::Regular, 'a');
    assert_eq!(plain.glyph(Style::Bold, 'a'), regular);
    assert_eq!(plain.atlas().len(), 1);
}

#[test]
fn padded_strides_larger_scales_and_extreme_origins_are_safe() {
    // Scale three with twelve bytes of padding a row: the padding is never
    // written, and neither is anything for an origin far off the surface.
    let s = surface(40, 30, 3);
    let stride = 40 * 4 + 12;
    let mut face = Face::new(face_bytes(false), None, 60).unwrap();
    let font = pinned().unwrap();
    let mut pixels = vec![GARBAGE; stride * 30];
    {
        let mut raster = Raster::new(&mut pixels, &font, s, stride)
            .unwrap()
            .with_face(&mut face);
        for (x, y) in [
            (i64::MIN, i64::MIN),
            (i64::MAX, i64::MAX),
            (i64::MIN, 0),
            (0, i64::MAX),
            (-1_000_000, 5),
        ] {
            raster.draw(glyph(x, y, 'T', Weight::Regular));
            raster.draw(glyph(x, y, 'Z', Weight::Medium));
        }
    }
    assert!(pixels.iter().all(|&b| b == GARBAGE));
    {
        let mut raster = Raster::new(&mut pixels, &font, s, stride)
            .unwrap()
            .with_face(&mut face);
        raster.draw(glyph(0, 0, 'T', Weight::Regular));
        raster.draw(glyph(30, 0, 'Z', Weight::Regular));
    }
    for row in pixels.chunks(stride) {
        assert!(row[160..].iter().all(|&b| b == GARBAGE), "padding");
    }
    assert!(pixels.iter().any(|&b| b != GARBAGE));
}
