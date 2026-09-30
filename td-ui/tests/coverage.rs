#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! Pixel and area oracles for the coverage rasterizer over hand-built
//! outlines: exact fills, half-pixel edges, winding, clamping, curve area
//! against the closed form, start-point independence and every refusal.

use td_ui::coverage::{Mask, Rasterizer, MAX_MASK_AXIS, MAX_OVERSAMPLED_AXIS};
use td_ui::raster::Error;
use td_ui::sfnt::{Outline, Point};

fn on(x: f32, y: f32) -> Point {
    Point { x, y, on: true }
}

fn off(x: f32, y: f32) -> Point {
    Point { x, y, on: false }
}

fn outline(contours: &[&[Point]]) -> Outline {
    let mut outline = Outline::new();
    for contour in contours {
        outline.push_contour(contour).unwrap();
    }
    outline
}

fn cover(outline: &Outline, scale: f32) -> Mask {
    let mut mask = Mask::default();
    Rasterizer::new()
        .rasterize(outline, scale, &mut mask)
        .unwrap();
    assert_eq!(mask.alpha.len(), mask.width * mask.height);
    mask
}

fn rectangle(left: f32, bottom: f32, right: f32, top: f32) -> [Point; 4] {
    // Clockwise with y up: TrueType's outer direction.
    [
        on(left, bottom),
        on(left, top),
        on(right, top),
        on(right, bottom),
    ]
}

fn area(mask: &Mask) -> f64 {
    mask.alpha.iter().map(|&a| f64::from(a) / 255.0).sum()
}

#[test]
fn an_aligned_square_is_exactly_covered_and_placed() {
    let mask = cover(&outline(&[&rectangle(100.0, -200.0, 900.0, 600.0)]), 0.01);
    assert_eq!((mask.width, mask.height, mask.left, mask.top), (8, 8, 1, 6));
    assert!(mask.alpha.iter().all(|&a| a == 255), "{:?}", mask.alpha);
}

#[test]
fn half_pixel_edges_are_half_covered() {
    let mask = cover(&outline(&[&rectangle(50.0, 0.0, 850.0, 400.0)]), 0.01);
    assert_eq!((mask.width, mask.height, mask.left, mask.top), (9, 4, 0, 4));
    for y in 0..4 {
        let row: Vec<u8> = (0..9).map(|x| mask.get(x, y)).collect();
        assert_eq!(row, [128, 255, 255, 255, 255, 255, 255, 255, 128]);
    }
    let mask = cover(&outline(&[&rectangle(0.0, 25.0, 300.0, 275.0)]), 0.01);
    assert_eq!((mask.width, mask.height, mask.top), (3, 3, 3));
    let column: Vec<u8> = (0..3).map(|y| mask.get(1, y)).collect();
    assert_eq!(column, [191, 255, 191]);
}

#[test]
fn direction_does_not_matter_and_holes_and_overlaps_follow_the_winding() {
    let square = rectangle(0.0, 0.0, 1000.0, 1000.0);
    let mut reversed = square;
    reversed.reverse();
    assert_eq!(
        cover(&outline(&[&square]), 0.01),
        cover(&outline(&[&reversed]), 0.01)
    );

    let mut hole = rectangle(300.0, 300.0, 700.0, 700.0);
    hole.reverse();
    let mask = cover(&outline(&[&square, &hole]), 0.01);
    for y in 0..10 {
        for x in 0..10 {
            let inside = (3..7).contains(&x) && (3..7).contains(&y);
            assert_eq!(mask.get(x, y), if inside { 0 } else { 255 }, "({x}, {y})");
        }
    }

    // A second contour in the same direction adds winding; coverage clamps.
    let inner = rectangle(300.0, 300.0, 700.0, 700.0);
    let mask = cover(&outline(&[&square, &inner]), 0.01);
    assert!(mask.alpha.iter().all(|&a| a == 255));
}

#[test]
fn a_triangle_covers_its_area() {
    let triangle = [on(0.0, 0.0), on(1300.0, 3700.0), on(4100.0, 900.0)];
    let mask = cover(&outline(&[&triangle]), 0.01);
    assert_eq!(
        (mask.width, mask.height, mask.left, mask.top),
        (41, 37, 0, 37)
    );
    let expected = 0.5 * f64::abs(13.0 * 9.0 - 37.0 * 41.0);
    assert!(
        (area(&mask) - expected).abs() < 0.1,
        "{} vs {expected}",
        area(&mask)
    );
    assert!(
        mask.alpha.iter().any(|&a| a != 0 && a != 255),
        "antialiased edges"
    );
    // Every pixel against a 64x64 point-sampled reference, the vertex
    // pixels (where two edges share a pixel) included.
    let corners = [(0.0, 37.0), (13.0, 0.0), (41.0, 28.0)];
    let side = |(ax, ay): (f64, f64), (bx, by): (f64, f64), x: f64, y: f64| {
        (bx - ax) * (y - ay) - (by - ay) * (x - ax)
    };
    for y in 0..mask.height {
        for x in 0..mask.width {
            let mut inside = 0;
            for sy in 0..64 {
                for sx in 0..64 {
                    let (px, py) = (
                        x as f64 + (f64::from(sx) + 0.5) / 64.0,
                        y as f64 + (f64::from(sy) + 0.5) / 64.0,
                    );
                    let signs = [
                        side(corners[0], corners[1], px, py),
                        side(corners[1], corners[2], px, py),
                        side(corners[2], corners[0], px, py),
                    ];
                    if signs.iter().all(|&s| s >= 0.0) || signs.iter().all(|&s| s <= 0.0) {
                        inside += 1;
                    }
                }
            }
            let reference = f64::from(inside) / 4096.0;
            let got = f64::from(mask.get(x, y)) / 255.0;
            assert!(
                (got - reference).abs() < 3.0 / 255.0,
                "({x}, {y}): {got} vs {reference}"
            );
        }
    }
}

#[test]
fn a_curve_covers_the_parabolic_segment() {
    // Chord along the bottom and one control point: the enclosed area is
    // two thirds of the control triangle's, whatever the flattening does.
    let segment = [on(0.0, 0.0), off(10000.0, 20000.0), on(20000.0, 0.0)];
    let mask = cover(&outline(&[&segment]), 0.01);
    let expected = 2.0 / 3.0 * 0.5 * 200.0 * 200.0;
    let relative = (area(&mask) - expected).abs() / expected;
    assert!(relative < 0.002, "{} vs {expected}", area(&mask));
    assert_eq!((mask.width, mask.height, mask.top), (200, 200, 200));
}

#[test]
fn an_all_off_curve_contour_implies_its_on_curve_midpoints() {
    // Four off-curve corners make a closed curve through the edge
    // midpoints; its area is the square's less four parabolic caps.
    let square = [
        off(0.0, 0.0),
        off(0.0, 10000.0),
        off(10000.0, 10000.0),
        off(10000.0, 0.0),
    ];
    let mask = cover(&outline(&[&square]), 0.01);
    let diamond = 0.5 * 100.0 * 100.0;
    let caps = 4.0 * (2.0 / 3.0) * (0.5 * 100.0 * 25.0);
    let expected = diamond + caps;
    assert!(
        (area(&mask) - expected).abs() / expected < 0.002,
        "{}",
        area(&mask)
    );
}

#[test]
fn the_contour_start_point_does_not_matter() {
    let contour = [
        on(0.0, 0.0),
        off(0.0, 800.0),
        on(400.0, 900.0),
        off(900.0, 900.0),
        off(900.0, 300.0),
        on(500.0, 0.0),
    ];
    let reference = cover(&outline(&[&contour]), 0.037);
    for shift in 1..contour.len() {
        let mut rotated = contour;
        rotated.rotate_left(shift);
        let mask = cover(&outline(&[&rotated]), 0.037);
        assert_eq!(
            (mask.width, mask.height, mask.left, mask.top),
            (
                reference.width,
                reference.height,
                reference.left,
                reference.top
            )
        );
        for (a, b) in mask.alpha.iter().zip(&reference.alpha) {
            assert!(a.abs_diff(*b) <= 1, "rotation {shift}");
        }
    }
}

#[test]
fn empty_and_degenerate_outlines_are_empty_masks() {
    let mut mask = Mask {
        width: 3,
        height: 1,
        left: 5,
        top: 5,
        alpha: vec![1, 2, 3],
    };
    Rasterizer::new()
        .rasterize(&Outline::new(), 1.0, &mut mask)
        .unwrap();
    assert_eq!(mask, Mask::default());
    let line = outline(&[&[on(0.0, 0.0), on(0.0, 500.0)]]);
    assert_eq!(cover(&line, 0.01), Mask::default());
    let dot = outline(&[&[on(5.0, 5.0)]]);
    assert_eq!(cover(&dot, 1.0), Mask::default());
}

#[test]
fn refusals_leave_the_mask_untouched() {
    let square = outline(&[&rectangle(0.0, 0.0, 1000.0, 1000.0)]);
    let before = cover(&square, 0.01);
    let mut rasterizer = Rasterizer::new();
    for scale in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        let mut mask = before.clone();
        assert_eq!(
            rasterizer.rasterize(&square, scale, &mut mask),
            Err(Error::InvalidArgument)
        );
        assert_eq!(mask, before);
    }
    let axis = MAX_MASK_AXIS as f32;
    let mut mask = before.clone();
    assert_eq!(
        rasterizer.rasterize(&square, (axis + 1.0) / 1000.0, &mut mask),
        Err(Error::Limit)
    );
    assert_eq!(mask, before);
    let exact = cover(&square, axis / 1000.0);
    assert_eq!((exact.width, exact.height), (MAX_MASK_AXIS, MAX_MASK_AXIS));
    let far = outline(&[&rectangle(1e30, 0.0, 1e30 + 1.0, 1.0)]);
    assert_eq!(
        rasterizer.rasterize(&far, 1.0, &mut mask),
        Err(Error::Limit)
    );
    let overflow = outline(&[&rectangle(0.0, 0.0, f32::MAX, 1.0)]);
    assert_eq!(
        rasterizer.rasterize(&overflow, 2.0, &mut mask),
        Err(Error::Limit)
    );
    assert_eq!(mask, before);
}

#[test]
fn the_rasterizer_is_reusable_across_sizes() {
    let big = outline(&[&rectangle(0.0, 0.0, 1000.0, 1000.0)]);
    let small = outline(&[&rectangle(0.0, 0.0, 250.0, 250.0)]);
    let mut rasterizer = Rasterizer::new();
    let mut mask = Mask::default();
    rasterizer.rasterize(&big, 0.1, &mut mask).unwrap();
    rasterizer.rasterize(&small, 0.01, &mut mask).unwrap();
    assert_eq!(mask, cover(&small, 0.01));
}

#[test]
fn flagged_overlaps_resolve_on_the_finer_grid() {
    // Two identical half-pixel rectangles: summed, the edge reads full;
    // flagged, each grid pixel clamps first and the union reads half.
    let half = rectangle(25.0, 0.0, 75.0, 100.0);
    let mut both = outline(&[&half, &half]);
    assert_eq!(cover(&both, 0.01).alpha, [255]);
    both.set_overlap();
    assert_eq!(cover(&both, 0.01).alpha, [128]);
    // Partly overlapping: the union covers nine tenths.
    let a = rectangle(0.0, 0.0, 60.0, 100.0);
    let b = rectangle(40.0, 0.0, 90.0, 100.0);
    let mut partly = outline(&[&a, &b]);
    partly.set_overlap();
    assert_eq!(cover(&partly, 0.01).alpha, [230]);
    // A flagged outline without overlap covers as it would unflagged:
    // without curves, areas add exactly, so only rounding differs.
    let triangle = [on(0.0, 0.0), on(1300.0, 3700.0), on(4100.0, 900.0)];
    let plain = cover(&outline(&[&triangle]), 0.01);
    let mut flagged = outline(&[&triangle]);
    flagged.set_overlap();
    let fine = cover(&flagged, 0.01);
    assert_eq!(
        (fine.width, fine.height, fine.left, fine.top),
        (plain.width, plain.height, plain.left, plain.top)
    );
    for (a, b) in fine.alpha.iter().zip(&plain.alpha) {
        assert!(a.abs_diff(*b) <= 1, "{a} {b}");
    }
    assert!((area(&fine) - area(&plain)).abs() < 0.2);
    // Past the finer grid's axis the outline is covered unrefined.
    let axis = MAX_OVERSAMPLED_AXIS as f32 + 1.0;
    let wide = rectangle(0.0, 0.0, axis * 100.0, 100.0);
    let mut large = outline(&[&half, &half, &wide]);
    let summed = cover(&large, 0.01);
    large.set_overlap();
    assert_eq!(cover(&large, 0.01), summed);
}

#[test]
fn a_mask_reads_zero_outside_itself() {
    let mask = Mask {
        width: 3,
        height: 2,
        left: 0,
        top: 0,
        alpha: vec![1, 2, 3, 4, 5, 6],
    };
    assert_eq!((mask.get(2, 1), mask.get(0, 1)), (6, 4));
    assert_eq!(mask.get(3, 0), 0);
    assert_eq!(mask.get(0, 2), 0);
    assert_eq!(mask.get(1, usize::MAX / 3), 0);
    assert_eq!(mask.get(usize::MAX, usize::MAX), 0);
}
