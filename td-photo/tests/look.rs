#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The look format: its refusals by line, its arithmetic against
//! hand-computed values, the built-in set, the look in the pipeline, and
//! the `looks` verb and `develop --look` run as the built binary.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use td_photo::color::{camera_color, Transfer, LUMA, MIDDLE_GREY};
use td_photo::develop::{self, Level1, Params};
use td_photo::look::{self, Error, Look, BUILTIN, HEADER, MAX_LOOK_BYTES, MAX_OPERATIONS};
use td_photo::{camera, library};

const NOT_TEXT: &str = "not text (a control character or a byte that is not UTF-8)";

fn parse(body: &str) -> Result<Look, Error> {
    Look::parse(format!("{HEADER}\n{body}").as_bytes())
}

fn look(body: &str) -> Look {
    parse(body).unwrap()
}

fn close(a: f32, b: f32, tolerance: f32) -> bool {
    (a - b).abs() <= tolerance
}

fn luma(rgb: [f32; 3]) -> f32 {
    LUMA[0] * rgb[0] + LUMA[1] * rgb[1] + LUMA[2] * rgb[2]
}

/// Zero, one, middle grey, a half and 200 values log-spaced from 2^-16 to
/// 1: every octave of the table's domain, between its nodes, ascending.
fn sweep() -> Vec<f32> {
    let mut xs = vec![0.0, 1.0, 0.5, MIDDLE_GREY];
    for i in 1..200 {
        xs.push(2f32.powf(-16.0 * (1.0 - i as f32 / 200.0)));
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs
}

/// A neutral value through a look, which keeps it neutral.
fn grey_through(look: &Look, v: f32) -> f32 {
    let [r, g, b] = look.apply([v; 3]);
    assert!(close(r, g, 1e-5) && close(g, b, 1e-5), "{v} -> {r} {g} {b}");
    r
}

// ---------------------------------------------------------------- format

#[test]
fn a_look_is_the_header_a_name_and_operations() {
    assert_eq!(Look::parse(b""), Err(Error::Header));
    assert_eq!(Look::parse(b"td-photo look 2\n"), Err(Error::Header));
    assert_eq!(Look::parse(b"td-photo look 1 \n"), Err(Error::Header));
    // The header alone, with or without its newline, is the empty look.
    let empty = Look::parse(b"td-photo look 1").unwrap();
    assert_eq!(Look::parse(b"td-photo look 1\n").unwrap(), empty);
    assert_eq!(empty.operations(), 0);
    assert_eq!(empty.name(), None);
    assert_eq!(empty.apply([0.3, 0.2, 0.1]), [0.3, 0.2, 0.1]);
    let named = look("name  Spaced name \nsaturation 1\n");
    assert_eq!(named.name(), Some("Spaced name"));
    assert_eq!(named.operations(), 1);
    // The name is not an operation.
    let sixteen = format!("name x\n{}", "saturation 1\n".repeat(MAX_OPERATIONS));
    assert_eq!(look(&sixteen).operations(), 16);
    // Sixteen points is the budget, not past it.
    let points: String = (0..16)
        .map(|i| format!(" {} {}", i as f32 / 15.0, i as f32 / 15.0))
        .collect();
    assert_eq!(look(&format!("curve luma{points}\n")).operations(), 1);
}

#[test]
fn the_designs_example_parses_and_is_the_designs() {
    let example = concat!(
        "td-photo look 1\n",
        "name Classic Chrome-like\n",
        "primaries 0.92 0.06 0.02  0.03 0.94 0.03  0.02 0.06 0.92\n",
        "tone contrast 1.35 toe 0.0 shoulder 0.0\n",
        "curve luma 0 0  0.25 0.22  0.75 0.78  1 1\n",
        "saturation 0.85\n",
        "monochrome 0.30 0.59 0.11\n",
    );
    let look = Look::parse(example.as_bytes()).unwrap();
    assert_eq!(look.name(), Some("Classic Chrome-like"));
    assert_eq!(look.operations(), 5);
    let design = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/DESIGN.md")).unwrap();
    assert!(
        design.contains(&format!("```text\n{example}```")),
        "the block drifted"
    );
}

#[test]
fn refusals_name_the_line() {
    let line = |number: usize, why: &'static str| Err(Error::Line(number, why));
    let cases: &[(&str, Result<Look, Error>)] = &[
        ("\n", line(2, "blank or indented line")),
        (" saturation 1\n", line(2, "blank or indented line")),
        (
            "saturation 1\n\nsaturation 1\n",
            line(3, "blank or indented line"),
        ),
        ("bogus 1\n", line(2, "unknown operation")),
        ("Saturation 1\n", line(2, "unknown operation")),
        ("name\n", line(2, "name is 1 to 64 characters")),
        ("name \n", line(2, "name is 1 to 64 characters")),
        ("name a\nname b\n", line(3, "a second name")),
        ("saturation\n", line(2, "saturation takes one number")),
        ("saturation 1 2\n", line(2, "saturation takes one number")),
        ("saturation 2.5\n", line(2, "saturation is 0 to 2")),
        ("saturation -0.1\n", line(2, "saturation is 0 to 2")),
        (
            "monochrome 1 1\n",
            line(2, "monochrome takes three weights"),
        ),
        (
            "monochrome -1 1 1\n",
            line(2, "monochrome weights are not negative and sum above zero"),
        ),
        (
            "monochrome 0 0 0\n",
            line(2, "monochrome weights are not negative and sum above zero"),
        ),
        ("primaries 1 0 0\n", line(2, "primaries takes nine numbers")),
        (
            "primaries 1 0 0 0 1 0 0 0 x\n",
            line(2, "primaries takes nine numbers"),
        ),
        (
            "primaries 5 -4 0 0 1 0 0 0 1\n",
            line(
                2,
                "primaries entries are within -4 to 4 once the row sums to one",
            ),
        ),
        (
            "primaries 0.001 0 -0.0009 0 1 0 0 0 1\n",
            line(
                2,
                "primaries entries are within -4 to 4 once the row sums to one",
            ),
        ),
        (
            "primaries 1 -1 0 0 1 0 0 0 1\n",
            line(2, "primaries rows sum above zero"),
        ),
        (
            "primaries 1 0 0 0 1 0 0 0 0\n",
            line(2, "primaries rows sum above zero"),
        ),
        (
            "tone 1\n",
            line(2, "tone takes contrast C toe T shoulder S"),
        ),
        (
            "tone contrast 1 toe 0 shoulder 0 extra\n",
            line(2, "tone takes contrast C toe T shoulder S"),
        ),
        (
            "tone contrast x toe 0 shoulder 0\n",
            line(2, "tone takes contrast C toe T shoulder S"),
        ),
        (
            "tone toe 0 contrast 1 shoulder 0\n",
            line(2, "tone takes contrast C toe T shoulder S"),
        ),
        (
            "tone contrast 0.4 toe 0 shoulder 0\n",
            line(2, "contrast is 0.5 to 3"),
        ),
        (
            "tone contrast 3.1 toe 0 shoulder 0\n",
            line(2, "contrast is 0.5 to 3"),
        ),
        (
            "tone contrast 1 toe 1.5 shoulder 0\n",
            line(2, "toe and shoulder are -1 to 1"),
        ),
        (
            "tone contrast 1 toe 0 shoulder -2\n",
            line(2, "toe and shoulder are -1 to 1"),
        ),
        ("curve x 0 0 1 1\n", line(2, "curve takes r, g, b or luma")),
        ("curve 0 0 1 1\n", line(2, "curve takes r, g, b or luma")),
        ("curve r a b\n", line(2, "curve takes pairs of numbers")),
        ("curve r 0 0 1\n", line(2, "curve takes 2 to 16 points")),
        ("curve r 0 0\n", line(2, "curve takes 2 to 16 points")),
        (
            "curve r 0 0 1 2\n",
            line(2, "curve points are within 0 to 1"),
        ),
        (
            "curve r -0.5 0 1 1\n",
            line(2, "curve points are within 0 to 1"),
        ),
        ("curve r 0 0 0 1\n", line(2, "curve points ascend in x")),
        ("curve r 0.5 0 0.2 1\n", line(2, "curve points ascend in x")),
    ];
    for (body, expected) in cases {
        assert_eq!(&parse(body), expected, "{body:?}");
    }
    // Seventeen points, and seventeen operations, one past each budget.
    let points: String = (0..17)
        .map(|i| format!(" {} {}", i as f32 / 16.0, i as f32 / 16.0))
        .collect();
    assert_eq!(
        parse(&format!("curve luma{points}\n")),
        line(2, "curve takes 2 to 16 points")
    );
    assert_eq!(
        parse(&"saturation 1\n".repeat(MAX_OPERATIONS + 1)),
        line(18, "over 16 operations")
    );
    // The ceiling is on the bytes, before anything is read as text.
    assert_eq!(
        Look::parse(&vec![b'a'; MAX_LOOK_BYTES + 1]),
        Err(Error::Size)
    );
    let padded = format!("{HEADER}\n{}", "saturation 1\n".repeat(16));
    assert!(padded.len() < MAX_LOOK_BYTES);
    let filled = format!(
        "{padded}name {}\n",
        "x".repeat(MAX_LOOK_BYTES - padded.len() - "name \n".len())
    );
    assert_eq!(filled.len(), MAX_LOOK_BYTES);
    // Exactly the ceiling is read; the name it holds is over its own.
    assert_eq!(
        Look::parse(filled.as_bytes()),
        line(18, "name is 1 to 64 characters")
    );
    // Not text, by the line it is on: a control character, a CRLF ending,
    // a byte that is not UTF-8; before the header is looked at.
    assert_eq!(parse("name a\tb\n"), line(2, NOT_TEXT));
    assert_eq!(parse("saturation 1\r\n"), line(2, NOT_TEXT));
    assert_eq!(parse("saturation 1\nname a\tb\n"), line(3, NOT_TEXT));
    assert_eq!(
        Look::parse(b"td-photo look 1\nname \xff\n"),
        line(2, NOT_TEXT)
    );
    assert_eq!(
        Look::parse(b"td-photo look 1\nsaturation 1\nname \xff\n"),
        line(3, NOT_TEXT)
    );
    assert_eq!(
        Look::parse(b"td-photo look 2\nname \xff\n"),
        line(2, NOT_TEXT)
    );
}

#[test]
fn numbers_are_plain_decimals() {
    for good in ["1", "-0", "0.5", "0.25", "2", "0", "00.50"] {
        assert!(parse(&format!("saturation {good}\n")).is_ok(), "{good}");
    }
    // Negatives where the range admits them.
    assert!(parse("tone contrast 1 toe -0.25 shoulder -1\n").is_ok());
    for bad in ["1.", ".5", "1e0", "+1", "1.0.0", "inf", "nan", "0x1", "1_0"] {
        assert_eq!(
            parse(&format!("saturation {bad}\n")),
            Err(Error::Line(2, "saturation takes one number")),
            "{bad}"
        );
    }
    // At most 24 characters, however many digits.
    assert!(parse(&format!("saturation 0.{}\n", "5".repeat(22))).is_ok());
    assert_eq!(
        parse(&format!("saturation 0.{}\n", "5".repeat(23))),
        Err(Error::Line(2, "saturation takes one number"))
    );
}

#[test]
fn errors_display_the_line_and_the_ceilings() {
    assert_eq!(Error::Size.to_string(), "over 4096 bytes");
    assert_eq!(
        Error::Header.to_string(),
        "first line is not `td-photo look 1`"
    );
    assert_eq!(
        Error::Line(3, "unknown operation").to_string(),
        "line 3: unknown operation"
    );
}

// ------------------------------------------------------------ arithmetic

#[test]
fn the_identity_tone_and_curve_are_the_identity_within_the_table() {
    let tone = look("tone contrast 1 toe 0 shoulder 0\n");
    let luma_curve = look("curve luma 0 0  1 1\n");
    let channels = look("curve r 0 0  1 1\ncurve g 0 0  1 1\ncurve b 0 0  1 1\n");
    for x in sweep() {
        assert!(close(grey_through(&tone, x), x, 1e-5), "{x}");
        assert!(close(grey_through(&luma_curve, x), x, 1e-5), "{x}");
        assert!(close(grey_through(&channels, x), x, 1e-5), "{x}");
    }
    // A colour through the luma identity keeps its ratios.
    let [r, g, b] = luma_curve.apply([0.4, 0.2, 0.1]);
    assert!(close(r, 0.4, 1e-5) && close(g, 0.2, 1e-5) && close(b, 0.1, 1e-5));
    // Past one, and below zero, a curve reads its input clamped.
    assert!(close(grey_through(&tone, 1.5), 1.0, 1e-6));
    assert!(close(grey_through(&tone, -0.5), 0.0, 1e-6));
    assert!(close(grey_through(&tone, f32::NAN), 0.0, 1e-6));
}

#[test]
fn a_tone_keeps_black_grey_and_white_and_is_monotone() {
    // Contrast 2 is `y = (1 + g) x^2 / (x^2 + g)`, since `k` is `g` there.
    let g = MIDDLE_GREY;
    let tone = look("tone contrast 2 toe 0 shoulder 0\n");
    assert!(close(grey_through(&tone, 0.0), 0.0, 1e-6));
    assert!(close(grey_through(&tone, g), g, 1e-4));
    assert!(close(grey_through(&tone, 1.0), 1.0, 1e-6));
    let expected = |x: f32| (1.0 + g) * x * x / (x * x + g);
    for x in [0.02, 0.05, 0.1, 0.3, 0.5, 0.7, 0.9] {
        assert!(close(grey_through(&tone, x), expected(x), 1e-3), "{x}");
    }
    assert!(close(grey_through(&tone, 0.5), 0.6815, 1e-3));
    assert!(grey_through(&tone, 0.05) < 0.05);
    // Monotone over the sweep, and so for every contrast in the range.
    for contrast in ["0.5", "0.8", "1.35", "2", "3"] {
        let tone = look(&format!("tone contrast {contrast} toe 0 shoulder 0\n"));
        let mut last = -1.0;
        for x in sweep() {
            let y = grey_through(&tone, x);
            assert!(y >= last - 1e-7, "{contrast}: {x} -> {y} after {last}");
            assert!((0.0..=1.0).contains(&y));
            last = y;
        }
        assert!(close(grey_through(&tone, g), g, 1e-4), "{contrast}");
        assert!(close(grey_through(&tone, 1.0), 1.0, 1e-6), "{contrast}");
    }
    // Soft contrast lifts the shadows and lowers the highlights.
    let soft = look("tone contrast 0.5 toe 0 shoulder 0\n");
    assert!(grey_through(&soft, 0.05) > 0.05);
    assert!(grey_through(&soft, 0.5) < 0.5);
}

#[test]
fn a_toe_shapes_below_grey_and_a_shoulder_above() {
    let g = MIDDLE_GREY;
    // The exponent eases from 1 at grey to the power at the end: a toe of
    // 1 (a power of 2 at black) is 1.25 halfway, so g/2 -> g 0.5^1.25.
    let toe = look("tone contrast 1 toe 1 shoulder 0\n");
    assert!(close(
        grey_through(&toe, g / 2.0),
        g * 0.5f32.powf(1.25),
        1e-3
    ));
    assert!(grey_through(&toe, g / 2.0) < g / 2.0);
    assert!(close(grey_through(&toe, g), g, 1e-4));
    assert!(close(grey_through(&toe, 0.5), 0.5, 1e-5));
    assert!(close(grey_through(&toe, 1.0), 1.0, 1e-6));
    // Flat at black: the power there is 2.
    assert!(grey_through(&toe, 1e-3) < 1e-4);
    // A negative one lifts: the exponent is 0.875 halfway.
    let lift = look("tone contrast 1 toe -1 shoulder 0\n");
    assert!(close(
        grey_through(&lift, g / 2.0),
        g * 0.5f32.powf(0.875),
        1e-3
    ));
    assert!(grey_through(&lift, g / 2.0) > g / 2.0);
    assert!(close(grey_through(&lift, 0.5), 0.5, 1e-5));
    // A positive shoulder lifts the highlights into white and is flat
    // there: 0.5 -> 1 - (1 - g) u^q for u the share of the rest left and
    // q = 1 + (1 - u)^2.
    let shoulder = look("tone contrast 1 toe 0 shoulder 1\n");
    let u = (1.0 - 0.5) / (1.0 - g);
    let expected = 1.0 - (1.0 - g) * u.powf(1.0 + (1.0 - u) * (1.0 - u));
    assert!(close(expected, 0.5353, 1e-3));
    assert!(close(grey_through(&shoulder, 0.5), expected, 1e-3));
    assert!(1.0 - grey_through(&shoulder, 0.999) < 1e-4);
    assert!(close(grey_through(&shoulder, 0.1), 0.1, 1e-5));
    assert!(close(grey_through(&shoulder, g), g, 1e-4));
    assert!(close(grey_through(&shoulder, 1.0), 1.0, 1e-6));
    // A negative one hardens them: the exponent is under 1.
    let hard = look("tone contrast 1 toe 0 shoulder -1\n");
    let expected = 1.0 - (1.0 - g) * u.powf(1.0 - 0.5 * (1.0 - u) * (1.0 - u));
    assert!(close(expected, 0.4813, 1e-3));
    assert!(close(grey_through(&hard, 0.5), expected, 1e-3));
    assert!(grey_through(&hard, 0.5) < 0.5);
    // The join at grey is smooth whatever the two are: the slopes just
    // below and just above agree within the table's precision.
    let h = 4e-3;
    for (t, s) in [(1, 1), (1, -1), (-1, 1), (-1, -1), (1, 0), (0, -1)] {
        let tone = look(&format!("tone contrast 1.5 toe {t} shoulder {s}\n"));
        let below = (grey_through(&tone, g) - grey_through(&tone, g - h)) / h;
        let above = (grey_through(&tone, g + h) - grey_through(&tone, g)) / h;
        assert!(close(below / above, 1.0, 0.02), "{t} {s}: {below} {above}");
    }
    // And each half is monotone across the corners of the ranges.
    for (t, s) in [(1, 1), (1, -1), (-1, 1), (-1, -1)] {
        let tone = look(&format!("tone contrast 1 toe {t} shoulder {s}\n"));
        let mut last = -1.0;
        for x in sweep() {
            let y = grey_through(&tone, x);
            assert!(y >= last - 1e-7, "{t} {s}: {x} -> {y} after {last}");
            last = y;
        }
    }
}

#[test]
fn a_curve_passes_its_points_and_holds_its_ends() {
    let red = look("curve r 0 0.1  0.5 0.5  1 0.9\n");
    assert!(close(red.apply([0.0; 3])[0], 0.1, 1e-3));
    assert!(close(red.apply([0.5; 3])[0], 0.5, 1e-3));
    assert!(close(red.apply([1.0; 3])[0], 0.9, 1e-3));
    // Only the named channel moves.
    assert_eq!(
        red.apply([0.5, 0.3, 0.2]),
        [red.apply([0.5; 3])[0], 0.3, 0.2]
    );
    assert_eq!(red.apply([0.0, 0.0, 0.0])[1..], [0.0, 0.0]);
    // Outside the first and last point the end value holds.
    let green = look("curve g 0.2 0.3  0.8 0.7\n");
    for x in [0.0, 0.1, 0.2] {
        assert!(close(green.apply([x; 3])[1], 0.3, 1e-3), "{x}");
    }
    for x in [0.8, 0.9, 1.0] {
        assert!(close(green.apply([x; 3])[1], 0.7, 1e-3), "{x}");
    }
    // Between two points the two-point curve is the line.
    assert!(close(green.apply([0.5; 3])[1], 0.5, 1e-3));
    // Monotone data give a monotone curve within 0..=1, no overshoot.
    let blue = look("curve b 0 0  0.1 0.9  0.2 0.95  1 1\n");
    let mut last = -1.0;
    for x in sweep() {
        let y = blue.apply([x; 3])[2];
        assert!((0.0..=1.0).contains(&y), "{x} -> {y}");
        assert!(y >= last - 1e-7, "{x} -> {y} after {last}");
        last = y;
    }
    assert!(close(blue.apply([0.1; 3])[2], 0.9, 1e-3));
}

#[test]
fn a_luminance_curve_scales_chroma_darkening_and_keeps_it_brightening() {
    // Darkening: the pixel is scaled, so its channel ratios (its hue and
    // saturation) hold.
    let darker = look("curve luma 0 0  0.5 0.25  1 1\n");
    let rgb = [0.4, 0.2, 0.1];
    let y = luma(rgb);
    let t = grey_through(&darker, y);
    assert!(t < y, "{t} {y}");
    let out = darker.apply(rgb);
    assert!(close(out[0] / out[1], 2.0, 1e-4));
    assert!(close(out[1] / out[2], 2.0, 1e-4));
    assert!(close(luma(out), t, 1e-5));
    // Brightening: the chroma is kept, so the channel differences hold
    // and the luminance lands on the curve.
    let brighter = look("curve luma 0 0  0.5 0.75  1 1\n");
    let t = grey_through(&brighter, y);
    assert!(t > y, "{t} {y}");
    let out = brighter.apply(rgb);
    assert!(close(out[0] - out[1], 0.2, 1e-5) && close(out[1] - out[2], 0.1, 1e-5));
    assert!(close(luma(out), t, 1e-5));
    // A lifted black: black becomes the grey at zero, a near-black hue
    // becomes nearly that grey (not a vivid colour), continuously.
    let lifted = look("curve luma 0 0.1  1 1\n");
    let black = lifted.apply([0.0; 3]);
    assert!(close(black[0], 0.1, 1e-3) && black[0] == black[1] && black[1] == black[2]);
    for red in [4e-6, 5e-6, 1e-4] {
        let out = lifted.apply([red, 0.0, 0.0]);
        for v in out {
            assert!(close(v, 0.1, 2e-3), "{red}: {out:?}");
        }
        assert!(out[0] > out[1] && out[1] == out[2], "{red}: {out:?}");
    }
    // Neutral through either rule is neutral, on the curve.
    for v in [0.0, 0.05, 0.5, 1.0] {
        assert!(close(grey_through(&lifted, v), 0.1 + 0.9 * v, 2e-3), "{v}");
    }
}

#[test]
fn saturation_mix_and_matrix_follow_their_formulas_in_file_order() {
    let rgb = [0.4, 0.2, 0.1];
    let y = luma(rgb);
    let none = look("saturation 0\n").apply(rgb);
    assert!(none.iter().all(|v| close(*v, y, 1e-6)), "{none:?}");
    let double = look("saturation 2\n").apply(rgb);
    for (out, v) in double.iter().zip(rgb) {
        assert!(close(*out, y + 2.0 * (v - y), 1e-6));
    }
    assert!(
        double[2] < 0.0,
        "chroma can leave the gamut; the transfer clips"
    );
    let same = look("saturation 1\n").apply(rgb);
    assert!(
        same.iter().zip(rgb).all(|(a, b)| close(*a, b, 1e-6)),
        "{same:?}"
    );
    // Weights renormalize: 1 1 2 is a quarter, a quarter and a half.
    let mono = look("monochrome 1 1 2\n").apply(rgb);
    assert!(mono.iter().all(|v| close(*v, 0.2, 1e-6)), "{mono:?}");
    // Rows renormalize: a scaled identity is the identity, and a row that
    // mixes keeps its sum.
    assert_eq!(look("primaries 2 0 0  0 2 0  0 0 2\n").apply(rgb), rgb);
    assert_eq!(look("primaries 0.0001 0 0  0 1 0  0 0 1\n").apply(rgb), rgb);
    let scaled = look("primaries 0.0001 -0.0001 0.0002  0 1 0  0 0 1\n").apply(rgb);
    assert!(close(scaled[0], 0.2 - 0.1 + 0.1, 1e-6), "{scaled:?}");
    let mixed = look("primaries 1 1 0  0 1 0  0 0 1\n").apply(rgb);
    assert!(close(mixed[0], 0.3, 1e-6) && mixed[1] == 0.2 && mixed[2] == 0.1);
    for v in [0.0, 0.25, 1.0] {
        let out = look("primaries 0.5 0.3 0.2  0.1 0.8 0.1  0.2 0.2 0.6\n").apply([v; 3]);
        assert!(out.iter().all(|c| close(*c, v, 1e-6)), "{v} -> {out:?}");
    }
    // File order: desaturate then mix takes the luminance; mix then
    // desaturate takes red.
    let first = look("saturation 0\nmonochrome 1 0 0\n").apply(rgb);
    let second = look("monochrome 1 0 0\nsaturation 0\n").apply(rgb);
    assert!(close(first[0], y, 1e-6), "{first:?}");
    assert!(close(second[0], 0.4, 1e-6), "{second:?}");
}

#[test]
fn sixteen_operations_keep_any_pixel_finite() {
    // The widest matrix row the range admits, sixteen times over, on a
    // pixel five stops over white: a gain of at most 12 per operation.
    let hostile = "primaries 4 -3 0  0 4 -3  -3 0 4\n".repeat(16);
    let out = look(&hostile).apply([32.0, 32.0, 32.0]);
    assert!(out.iter().all(|v| v.is_finite()), "{out:?}");
    let out = look(&hostile).apply([32.0, 0.0, -32.0]);
    assert!(out.iter().all(|v| v.is_finite()), "{out:?}");
    let saturated = "saturation 2\n".repeat(16);
    let out = look(&saturated).apply([32.0, 0.0, -32.0]);
    assert!(out.iter().all(|v| v.is_finite()), "{out:?}");
    // The knots the grammar admits can put a secant near 1e22 beside one
    // near 1e-22; the curve is still finite, within 0..=1 and monotone.
    let steep = look("curve r 0 1  0.0000000000000000000001 0.0000000000000000000001  1 0\n");
    let mut last = 2.0;
    for x in sweep() {
        let y = steep.apply([x; 3])[0];
        assert!(y.is_finite() && (0.0..=1.0).contains(&y), "{x} -> {y}");
        assert!(y <= last + 1e-7, "{x} -> {y} after {last}");
        last = y;
    }
    assert!(close(steep.apply([0.0; 3])[0], 1.0, 1e-6));
    assert!(close(steep.apply([1.0; 3])[0], 0.0, 1e-6));
}

// ------------------------------------------------------------- built-ins

#[test]
fn every_built_in_parses_within_the_budgets_and_keeps_neutral_neutral() {
    let stems: Vec<&str> = BUILTIN.iter().map(|(stem, _)| *stem).collect();
    assert_eq!(
        stems,
        [
            "contrast-boost",
            "contrast-soft",
            "mono",
            "provia-like",
            "velvia-like",
            "astia-like",
            "classic-chrome-like",
            "classic-neg-like",
            "eterna-like",
            "acros-like",
        ]
    );
    assert_eq!(stems.iter().collect::<BTreeSet<_>>().len(), stems.len());
    assert_eq!(look::builtin("nope"), None);
    for (stem, text) in BUILTIN {
        assert!(library::valid_look(stem), "{stem}");
        assert_eq!(look::builtin(stem), Some(text));
        assert!(text.len() <= MAX_LOOK_BYTES, "{stem}");
        assert!(text.starts_with("td-photo look 1\nname "), "{stem}");
        let look = Look::parse(text.as_bytes()).unwrap_or_else(|e| panic!("{stem}: {e}"));
        assert!(look.name().is_some(), "{stem}");
        assert!(
            look.operations() >= 1 && look.operations() <= MAX_OPERATIONS,
            "{stem}"
        );
        // A neutral ramp stays neutral, monotone, black and white fixed.
        let mut last = -1.0;
        for x in sweep() {
            let y = grey_through(&look, x);
            assert!(y >= last - 1e-6, "{stem}: {x} -> {y} after {last}");
            last = y;
        }
        assert!(close(grey_through(&look, 0.0), 0.0, 1e-6), "{stem}");
        assert!(close(grey_through(&look, 1.0), 1.0, 1e-4), "{stem}");
    }
    // The two monochromes make a colour grey; the rest keep some chroma.
    for (stem, text) in BUILTIN {
        let out = Look::parse(text.as_bytes()).unwrap().apply([0.4, 0.2, 0.1]);
        let grey = close(out[0], out[1], 1e-6) && close(out[1], out[2], 1e-6);
        assert_eq!(
            grey,
            stem == "mono" || stem == "acros-like",
            "{stem}: {out:?}"
        );
    }
}

// -------------------------------------------------------------- pipeline

fn level1_of(pixels: &[[f32; 3]], width: usize, height: usize) -> Level1 {
    Level1 {
        width,
        height,
        rgb: pixels
            .iter()
            .flat_map(|p| p.map(|v| (v * 65535.0 + 0.5) as u16))
            .collect(),
    }
}

#[test]
fn the_pipeline_applies_the_look_after_the_matrix() {
    let z8 = camera::find("NIKON CORPORATION", "NIKON Z 8").unwrap();
    let color = camera_color(&z8.xyz_to_cam).unwrap();
    let transfer = Transfer::srgb();
    let wb = color.daylight;
    let grey = |v: f32| [v / wb[0], v / wb[1], v / wb[2]];
    // A saturated red in camera space, past a clip in nothing.
    let red = [0.5 / wb[0], 0.05 / wb[1], 0.05 / wb[2]];
    let level1 = level1_of(&[grey(0.0), grey(MIDDLE_GREY), grey(1.0), red], 2, 2);
    let render = |look: Option<&Look>| {
        develop::render(
            &level1,
            None,
            2,
            1,
            wb,
            &color,
            &transfer,
            &Params {
                exposure: 0.0,
                threads: 2,
                look,
            },
        )
        .unwrap()
    };
    let plain = render(None);
    let boost = Look::parse(look::builtin("contrast-boost").unwrap().as_bytes()).unwrap();
    let boosted = render(Some(&boost));
    // Middle grey is the tone's fixed point: the same 8-bit value as
    // without the look, while black and white stay put and the colour
    // moves.
    assert_eq!(boosted.pixel(0, 0), Some([0, 0, 0]));
    assert_eq!(boosted.pixel(0, 1), Some([255, 255, 255]));
    let mid = boosted.pixel(1, 0).unwrap();
    let plain_mid = plain.pixel(1, 0).unwrap();
    assert_eq!(mid, plain_mid);
    for a in mid {
        assert!((116..=120).contains(&a), "{mid:?}");
    }
    assert_ne!(boosted.pixel(1, 1), plain.pixel(1, 1));
    // The mix makes the colour grey after the matrix, not before it.
    let mono = Look::parse(look::builtin("mono").unwrap().as_bytes()).unwrap();
    let mixed = render(Some(&mono));
    let [r, g, b] = mixed.pixel(1, 1).unwrap();
    assert!(r == g && g == b, "{r} {g} {b}");
    assert!(plain.pixel(1, 1).unwrap()[0] > plain.pixel(1, 1).unwrap()[1]);
    assert_eq!(mixed.pixel(1, 0), plain.pixel(1, 0));
}

// ------------------------------------------------------------ the binary

struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Temp {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "td-photo-look-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Temp(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs the binary with the two directory variables as given (`None`
/// removes one).
fn td_photo(args: &[&str], config: Option<&str>, home: Option<&str>) -> (bool, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_td-photo"));
    command.args(args);
    for (name, value) in [("XDG_CONFIG_HOME", config), ("HOME", home)] {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    let output = command.output().unwrap();
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn the_looks_verb_lists_user_and_built_in_looks_and_prints_one() {
    let temp = Temp::new("verb");
    let config = temp.0.join("config");
    let dir = config.join("td-photo/looks");
    fs::create_dir_all(&dir).unwrap();
    let mine = "td-photo look 1\nname Mine\nsaturation 1.2\n";
    fs::write(dir.join("mine.look"), mine).unwrap();
    fs::write(
        dir.join("mono.look"),
        "td-photo look 1\nname My mono\nmonochrome 1 1 1\n",
    )
    .unwrap();
    fs::write(dir.join("nameless.look"), "td-photo look 1\n").unwrap();
    fs::write(dir.join("broken.look"), "td-photo look 1\nbogus 1\n").unwrap();
    fs::write(dir.join("weird\tstem.look"), mine).unwrap();
    fs::write(dir.join("notes.txt"), "not a look").unwrap();
    fs::create_dir(dir.join("folder.look")).unwrap();
    // A link to nothing at a built-in's stem is the user's file, and
    // unreadable: neither listed as the built-in nor developed with it.
    std::os::unix::fs::symlink("nowhere", dir.join("eterna-like.look")).unwrap();
    // A link to a look is that look.
    std::os::unix::fs::symlink("mine.look", dir.join("linked.look")).unwrap();
    fs::write(dir.join("two\nlines.look"), mine).unwrap();
    fs::write(dir.join("big.look"), vec![b'a'; MAX_LOOK_BYTES + 1]).unwrap();
    let config_text = config.to_str().unwrap();

    let (ok, stdout, stderr) = td_photo(&["looks"], Some(config_text), None);
    assert!(ok, "{stderr}");
    assert_eq!(stderr, "");
    let rows: Vec<&str> = stdout.lines().collect();
    assert_eq!(rows.len(), BUILTIN.len() + 8, "{stdout}");
    assert!(
        rows.iter().all(|r| r.matches('\t').count() == 2),
        "{stdout}"
    );
    let mut sorted = rows.clone();
    sorted.sort();
    assert_eq!(rows, sorted, "by stem");
    assert!(rows.contains(&"mine\tuser\tMine"), "{stdout}");
    assert!(rows.contains(&"mono\tuser\tMy mono"), "{stdout}");
    assert!(rows.contains(&"nameless\tuser\t-"), "{stdout}");
    assert!(
        rows.contains(&"broken\tuser\terror line 2: unknown operation"),
        "{stdout}"
    );
    assert!(
        rows.iter()
            .any(|r| r.starts_with("\"weird\\tstem\"\tuser\terror not a look stem")),
        "{stdout}"
    );
    assert!(rows.contains(&"linked\tuser\tMine"), "{stdout}");
    assert!(
        rows.iter()
            .any(|r| r.starts_with("\"two\\nlines\"\tuser\terror not a look stem")),
        "{stdout}"
    );
    assert!(
        rows.contains(&"big\tuser\terror over 4096 bytes"),
        "{stdout}"
    );
    assert!(
        rows.contains(&"eterna-like\tuser\terror a link to nothing"),
        "{stdout}"
    );
    assert_eq!(
        rows.iter()
            .filter(|r| r.starts_with("eterna-like\t"))
            .count(),
        1
    );
    assert!(
        rows.iter()
            .any(|r| r.starts_with("folder\tuser\terror not a regular file")),
        "{stdout}"
    );
    assert!(
        rows.contains(&"contrast-boost\tbuilt-in\tContrast boost"),
        "{stdout}"
    );
    assert!(
        rows.contains(&"acros-like\tbuilt-in\tAcros-like"),
        "{stdout}"
    );
    assert_eq!(rows.iter().filter(|r| r.starts_with("mono\t")).count(), 1);

    // With a stem: the user's text, the built-in's without, a refusal by
    // file and line, and a stem the grammar refuses.
    let (ok, stdout, _) = td_photo(&["looks", "mine"], Some(config_text), None);
    assert!(ok);
    assert_eq!(stdout, mine);
    let (ok, stdout, _) = td_photo(&["looks", "mono"], Some(config_text), None);
    assert!(ok);
    assert_eq!(stdout, "td-photo look 1\nname My mono\nmonochrome 1 1 1\n");
    let home = temp.0.join("home");
    fs::create_dir_all(&home).unwrap();
    let (ok, stdout, _) = td_photo(&["looks", "mono"], None, home.to_str());
    assert!(ok);
    assert_eq!(stdout, look::builtin("mono").unwrap());
    let (ok, stdout, stderr) = td_photo(&["looks", "broken"], Some(config_text), None);
    assert!(!ok);
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("broken.look: line 2: unknown operation"),
        "{stderr}"
    );
    let (ok, stdout, stderr) = td_photo(&["looks", "eterna-like"], Some(config_text), None);
    assert!(!ok);
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("eterna-like.look: a link to nothing"),
        "{stderr}"
    );
    let (ok, _, stderr) = td_photo(&["looks", "nope"], Some(config_text), None);
    assert!(!ok);
    assert!(
        stderr.contains("look nope: not built in and not at"),
        "{stderr}"
    );
    let (ok, _, stderr) = td_photo(&["looks", "../mine"], Some(config_text), None);
    assert!(!ok);
    assert!(stderr.contains("is not a look stem"), "{stderr}");
    let (ok, _, stderr) = td_photo(&["looks", "a", "b"], Some(config_text), None);
    assert!(!ok);
    assert!(stderr.contains("looks takes at most STEM"), "{stderr}");

    // A home without a config directory lists the built-in set alone; no
    // resolvable directory says so on stderr and lists it too.
    let (ok, stdout, stderr) = td_photo(&["looks"], None, home.to_str());
    assert!(ok, "{stderr}");
    assert_eq!(stderr, "");
    assert_eq!(stdout.lines().count(), BUILTIN.len());
    assert!(
        stdout.lines().all(|r| r.contains("\tbuilt-in\t")),
        "{stdout}"
    );
    let (ok, stdout, stderr) = td_photo(&["looks"], Some("relative"), None);
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("user looks not listed: neither XDG_CONFIG_HOME nor HOME"),
        "{stderr}"
    );
    assert_eq!(stdout.lines().count(), BUILTIN.len());
    // A directory that cannot be read (a file in its place) the same way.
    let blocked = temp.0.join("blocked");
    fs::create_dir_all(blocked.join("td-photo")).unwrap();
    fs::write(blocked.join("td-photo/looks"), "not a directory").unwrap();
    let (ok, stdout, stderr) = td_photo(&["looks"], blocked.to_str(), None);
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("user looks not listed:") && stderr.contains("looks:"),
        "{stderr}"
    );
    assert_eq!(stdout.lines().count(), BUILTIN.len());
    // With a stem, that look's failure is the verb's.
    let (ok, _, stderr) = td_photo(&["looks", "mono"], blocked.to_str(), None);
    assert!(!ok);
    assert!(stderr.contains("mono.look:"), "{stderr}");
    let (ok, stdout, _) = td_photo(&["looks", "contrast-soft"], Some("relative"), None);
    assert!(ok);
    assert_eq!(stdout, look::builtin("contrast-soft").unwrap());
}

#[test]
fn develop_refuses_a_bad_look_before_reading_anything() {
    let temp = Temp::new("develop");
    let dir = temp.0.join("config/td-photo/looks");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("broken.look"),
        "td-photo look 1\nname Broken\ntone 1\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("nowhere", dir.join("mono.look")).unwrap();
    let config = temp.0.join("config");
    let config_text = config.to_str().unwrap();
    let missing = temp.0.join("missing.NEF");
    let out = temp.0.join("out.ppm");
    let missing = missing.to_str().unwrap();
    let out_text = out.to_str().unwrap();
    for (stem, why) in [
        ("../x", "is not a look stem"),
        ("nope", "look nope: not built in and not at"),
        (
            "broken",
            "broken.look: line 3: tone takes contrast C toe T shoulder S",
        ),
        ("mono", "mono.look: a link to nothing"),
    ] {
        let (ok, stdout, stderr) = td_photo(
            &["develop", missing, out_text, "--look", stem],
            Some(config_text),
            None,
        );
        assert!(!ok, "{stem}");
        assert_eq!(stdout, "");
        assert!(stderr.contains(why), "{stem}: {stderr}");
        assert!(
            !stderr.contains("missing.NEF"),
            "{stem}: refused before the read: {stderr}"
        );
    }
    assert!(!out.exists());
    // A good look gets as far as the camera file.
    let (ok, _, stderr) = td_photo(
        &["develop", missing, out_text, "--look", "acros-like"],
        Some(config_text),
        None,
    );
    assert!(!ok);
    assert!(stderr.contains("missing.NEF"), "{stderr}");
    let (ok, _, stderr) = td_photo(
        &["develop", missing, out_text, "--look"],
        Some(config_text),
        None,
    );
    assert!(!ok);
    assert!(stderr.contains("--look needs a value"), "{stderr}");
}
