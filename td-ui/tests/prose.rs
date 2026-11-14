#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! Prose: on the bitmap face, `Metrics` answering the cell, `wrap`
//! breaking by pixel width and a wrapped line's draw stream equal to a
//! text run's; over fonts `fonts` encodes, advances from the face that
//! draws each scalar, pens summed without drift, the raster drawing a
//! prose glyph at its pen and the line's baseline, clipped to its draw and
//! not to a cell, and falling back to the monospace face and the bitmap
//! face; the prose faces' loader over a directory the test writes; and,
//! ignored unless asked for, the same oracles over the pinned faces
//! where `./install-fonts` or the image puts them.

mod fonts;
// The shared temporary-tree guard (AGENTS.md "Parallel work"), compiled
// in because td-ui, the leaf, names no dependency.
#[path = "../../td-fs/src/temp_dir.rs"]
mod temp_dir;

use std::ffi::OsStr;
use std::fs;

use fonts::{Builder, Glyph, Segment};
use td_ui::atlas::Style;
use td_ui::driven;
use td_ui::face_file::Family;
use td_ui::font::pinned;
use td_ui::pinned_face::{load_styled_in, prose_from, prose_in, prose_in_or_note};
use td_ui::prose::{
    draw, Faces, Line, LineBox, Metrics, Refused, Run, Source, CELL_BASELINE, OVERHANG, SIZE, UNIT,
};
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Raster, Rect, Scale, Surface, TextFamily,
};
use td_ui::sfnt::Error;
use td_ui::typeface::Typeface;
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

const GARBAGE: u8 = 0x5a;
const INK: u32 = 0x102030;
const PAPER: u32 = 0xf0e0d0;

fn scale(value: u8) -> Scale {
    Scale::new(value).unwrap()
}

fn style(family: TextFamily) -> GlyphStyle {
    GlyphStyle::regular(INK, PAPER).with_family(family)
}

fn run(text: &str, family: TextFamily, tag: usize) -> Run<'_> {
    Run {
        text,
        style: style(family),
        tag,
    }
}

/// The text a line shows: its pieces' bytes of their runs.
fn shown(line: &Line, runs: &[Run]) -> String {
    line.pieces
        .iter()
        .map(|piece| &runs[piece.run].text[piece.text.clone()])
        .collect()
}

fn rectangle(left: i32, bottom: i32, right: i32, top: i32) -> Vec<(i32, i32, bool)> {
    vec![
        (left, bottom, true),
        (left, top, true),
        (right, top, true),
        (right, bottom, true),
    ]
}

/// A font mapping each scalar to its glyph, with each glyph's advance.
fn encoded(glyphs: &[(char, Glyph, u16)], ascender: i16, descender: i16) -> Vec<u8> {
    let mut sorted: Vec<(char, u16)> = glyphs
        .iter()
        .enumerate()
        .map(|(index, (scalar, _, _))| (*scalar, index as u16 + 1))
        .collect();
    sorted.sort();
    let mut builder = Builder::new(
        std::iter::once(Glyph::Empty)
            .chain(glyphs.iter().map(|(_, glyph, _)| glyph.clone()))
            .collect(),
    );
    builder.advances = std::iter::once(0)
        .chain(glyphs.iter().map(|(_, _, advance)| *advance))
        .collect();
    builder.format4 = sorted
        .into_iter()
        .map(|(scalar, glyph)| {
            let code = u32::from(scalar) as u16;
            Segment::Delta(code, code, glyph.wrapping_sub(code))
        })
        .collect();
    builder.ascender = ascender;
    builder.descender = descender;
    builder.font()
}

/// Sans: '0' a square, ' ' blank, 'i' 100 units wide, 'm' 700, 'T' a
/// full em from below the baseline, at 600, 250, 200, 800 and 1000 units,
/// and 'j' 400 wide hanging 300 left of its pen at 200; an 800 ascender
/// and a -300 descender, the deeper.
fn sans() -> Vec<u8> {
    encoded(
        &[
            ('0', Glyph::Simple(vec![rectangle(0, 0, 500, 500)]), 600),
            (' ', Glyph::Empty, 250),
            ('i', Glyph::Simple(vec![rectangle(0, 0, 100, 500)]), 200),
            ('m', Glyph::Simple(vec![rectangle(0, 0, 700, 500)]), 800),
            (
                'T',
                Glyph::Simple(vec![rectangle(0, -200, 1000, 800)]),
                1000,
            ),
            ('j', Glyph::Simple(vec![rectangle(-300, 0, 100, 500)]), 200),
        ],
        800,
        -300,
    )
}

/// Serif: as Sans with 'i' at 300 units, a 900 ascender, the higher, and
/// a -200 descender.
fn serif() -> Vec<u8> {
    encoded(
        &[
            ('0', Glyph::Simple(vec![rectangle(0, 0, 500, 500)]), 600),
            (' ', Glyph::Empty, 250),
            ('i', Glyph::Simple(vec![rectangle(0, 0, 100, 500)]), 300),
            ('m', Glyph::Simple(vec![rectangle(0, 0, 700, 500)]), 800),
        ],
        900,
        -200,
    )
}

/// The monospace face: '0' and '─' at 600 units, nothing else.
fn mono() -> Typeface {
    let bytes = encoded(
        &[
            ('0', Glyph::Simple(vec![rectangle(0, 0, 500, 500)]), 600),
            ('─', Glyph::Simple(vec![rectangle(0, 200, 600, 300)]), 600),
        ],
        800,
        -200,
    );
    Typeface::new(bytes, None).unwrap()
}

fn four(bytes: Vec<u8>) -> [Vec<u8>; 4] {
    [bytes.clone(), bytes.clone(), bytes.clone(), bytes]
}

fn faces() -> Faces {
    Faces::new(four(sans()), four(serif())).unwrap()
}

fn faces_with_mono() -> Faces {
    faces().with_mono(&mono()).unwrap()
}

/// The pixels a raster over a buffer of garbage leaves after `draws`,
/// with `prose` when given.
fn painted(surface: Surface, prose: Option<&mut Faces>, draws: &[Draw]) -> Vec<u8> {
    let font = pinned().unwrap();
    let mut pixels = vec![GARBAGE; surface.width * surface.height * 4];
    {
        let mut raster = Raster::new(&mut pixels, &font, surface, surface.width * 4)
            .unwrap()
            .with_prose(prose);
        for draw in draws {
            raster.draw(*draw);
        }
    }
    pixels
}

fn glyph(clip: Rect, (x, y): (i64, i64), scalar: char, style: GlyphStyle) -> Draw {
    Draw {
        clip,
        primitive: Primitive::Glyph {
            x,
            y,
            scalar,
            style,
        },
    }
}

fn pixel(pixels: &[u8], surface: Surface, (x, y): (usize, usize)) -> u32 {
    let at = (y * surface.width + x) * 4;
    u32::from_le_bytes(pixels[at..at + 4].try_into().unwrap())
}

fn untouched(pixels: &[u8], surface: Surface, at: (usize, usize)) -> bool {
    pixel(pixels, surface, at) == u32::from_le_bytes([GARBAGE; 4])
}

// On the bitmap face.

#[test]
fn without_faces_metrics_answer_the_cell() {
    for value in 1..=4 {
        let mut metrics = Metrics::new(None, scale(value));
        let size = usize::from(value);
        assert!(!metrics.proportional());
        assert_eq!(
            metrics.line(),
            LineBox {
                height: (CELL_HEIGHT * size) as u32,
                baseline: (CELL_BASELINE * size) as u32,
            }
        );
        for family in [
            TextFamily::Cell,
            TextFamily::Sans,
            TextFamily::Serif,
            TextFamily::Mono,
        ] {
            for scalar in ['i', 'm', ' ', '─', '\u{e0a0}'] {
                assert_eq!(
                    metrics.advance(family, Style::Bold, scalar),
                    (CELL_WIDTH * size) as u32 * UNIT
                );
                assert_eq!(metrics.source(family, Style::Regular, scalar), Source::Cell);
            }
            assert_eq!(
                metrics.width(family, Style::Regular, "hello"),
                (5 * CELL_WIDTH * size) as u32
            );
        }
    }
}

#[test]
fn on_the_cell_wrap_breaks_after_a_space_and_inside_a_word_too_long() {
    let mut metrics = Metrics::new(None, scale(1));
    let lay = |metrics: &mut Metrics, text: &str, width: u32| {
        let runs = [run(text, TextFamily::Sans, 0)];
        wrap(metrics, &runs, width)
            .iter()
            .map(|line| (shown(line, &runs), line.width))
            .collect::<Vec<_>>()
    };
    let own = |pairs: &[(&str, u32)]| -> Vec<(String, u32)> {
        pairs.iter().map(|(s, w)| (s.to_string(), *w)).collect()
    };
    // Ten cells: the space before "brown" is the break, and takes no
    // width at the line's end.
    assert_eq!(
        lay(&mut metrics, "the quick brown fox", 80),
        own(&[("the quick", 72), ("brown fox", 72)])
    );
    // Exactly full lines hold; a trailing run of spaces hangs.
    assert_eq!(
        lay(&mut metrics, "abcdefghij   klm", 80),
        own(&[("abcdefghij", 80), ("klm", 24)])
    );
    // A word alone wider than a line breaks inside it.
    assert_eq!(
        lay(&mut metrics, "ab abcdefghijkl", 80),
        own(&[("ab", 16), ("abcdefghij", 80), ("kl", 16)])
    );
    // Newlines end lines, an empty one included, and the text after the
    // last one is a line even when empty; leading spaces are kept.
    assert_eq!(
        lay(&mut metrics, "a\n\n  b\n", 80),
        own(&[("a", 8), ("", 0), ("  b", 24), ("", 0)])
    );
    assert_eq!(lay(&mut metrics, "", 80), own(&[("", 0)]));
    // A line never ends holding only spaces: those before a word moved
    // on go with the break, leading ones included.
    assert_eq!(
        lay(&mut metrics, "  abcdefghijkl", 80),
        own(&[("abcdefghij", 80), ("kl", 16)])
    );
    assert_eq!(lay(&mut metrics, " bbbb", 32), own(&[("bbbb", 32)]));
    assert_eq!(
        lay(&mut metrics, "a\n   bcdefgh", 48),
        own(&[("a", 8), ("bcdefg", 48), ("h", 8)])
    );
    // Spaces that fit stay as indentation.
    assert_eq!(lay(&mut metrics, "  abc", 48), own(&[("  abc", 40)]));
    // However narrow, a line holds a glyph.
    assert_eq!(
        lay(&mut metrics, "ab c", 0),
        own(&[("a", 8), ("b", 8), ("c", 8)])
    );
    // A tab is a space and another control the replacement character.
    let runs = [run("a\tb\u{7}", TextFamily::Serif, 0)];
    let lines = wrap(&mut metrics, &runs, 80);
    let scalars: String = lines[0].glyphs.iter().map(|g| g.scalar).collect();
    assert_eq!(scalars, "a b\u{fffd}");
}

#[test]
fn wrap_at_most_stops_at_its_lines_and_they_are_wraps_first() {
    let mut metrics = Metrics::new(None, scale(1));
    let text = "ab cd\nef gh ijklmnop\n\nq";
    let runs = [run(text, TextFamily::Sans, 0)];
    let all = wrap(&mut metrics, &runs, 24);
    assert_eq!(all.len(), 9);
    for most in 0..=all.len() + 2 {
        let some = wrap_at_most(&mut metrics, &runs, 24, most);
        assert_eq!(some, all[..most.min(all.len())], "{most}");
    }
    // However long the text, a narrow width lays out only what is kept.
    let long = "x".repeat(100_000);
    let runs = [run(&long, TextFamily::Sans, 0)];
    let lines = wrap_at_most(&mut metrics, &runs, 1, 3);
    assert_eq!(lines.len(), 3);
    assert!(lines.iter().all(|line| line.glyphs.len() == 1));
}

#[test]
fn on_the_cell_a_wrapped_line_streams_as_a_text_run_does() {
    let text = "the quick brown fox jumps over the lazy dog";
    for value in 1..=4 {
        let size = i64::from(value);
        let mut metrics = Metrics::new(None, scale(value));
        let width = (CELL_WIDTH as u32) * 12 * u32::from(value);
        let runs = [run(text, TextFamily::Sans, 0)];
        let lines = wrap(&mut metrics, &runs, width);
        assert_eq!(lines.len(), 4);
        let bounds = Rect {
            x: 5,
            y: 7,
            width: width + 3,
            height: 200,
        };
        let damage = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        };
        for (index, line) in lines.iter().enumerate() {
            let origin = (5, 7 + index as i64 * CELL_HEIGHT as i64 * size);
            let mut prose = Vec::new();
            draw(line, origin, bounds, damage, &mut |d| prose.push(d));
            let mut cells = Vec::new();
            text_run(
                scale(value),
                shown(line, &runs).chars(),
                origin,
                bounds,
                style(TextFamily::Sans),
                damage,
                &mut |d| cells.push(d),
            );
            assert_eq!(prose, cells, "line {index} at scale {value}");
        }
    }
}

#[test]
fn a_line_draws_nothing_off_its_damage_and_nothing_past_its_clip() {
    let mut metrics = Metrics::new(None, scale(1));
    let text = "a".repeat(30);
    let runs = [run(&text, TextFamily::Sans, 0)];
    let line = &wrap(&mut metrics, &runs, 1000)[0];
    let bounds = Rect {
        x: 0,
        y: 0,
        width: 20,
        height: 16,
    };
    let mut draws = Vec::new();
    draw(line, (0, 0), bounds, bounds, &mut |d| draws.push(d));
    // Pens before the clip's right and up to `OVERHANG` past it, whose
    // ink may hang into the clip: 0 through 80 of 232.
    assert_eq!(OVERHANG, 64);
    assert_eq!(draws.len(), 11);
    let far = Rect {
        x: 100,
        y: 100,
        width: 5,
        height: 5,
    };
    draws.clear();
    draw(line, (0, 0), bounds, far, &mut |d| draws.push(d));
    assert!(draws.is_empty());
}

#[test]
fn pieces_keep_their_runs_extents_and_tags_for_hit_testing() {
    let mut metrics = Metrics::new(None, scale(2));
    let runs = [
        run("see ", TextFamily::Sans, 3),
        run("this link", TextFamily::Serif, 7),
        run(" now", TextFamily::Sans, 3),
    ];
    let lines = wrap(&mut metrics, &runs, 16 * 10);
    let texts: Vec<String> = lines.iter().map(|line| shown(line, &runs)).collect();
    assert_eq!(texts, ["see this", "link now"]);
    let first = &lines[0];
    assert_eq!(
        first
            .pieces
            .iter()
            .map(|p| (
                p.run,
                p.tag,
                p.text.clone(),
                p.left,
                p.right,
                p.glyphs.clone()
            ))
            .collect::<Vec<_>>(),
        [(0, 3, 0..4, 0, 64, 0..4), (1, 7, 0..4, 64, 128, 4..8)]
    );
    assert_eq!(first.pieces[1].style.family, TextFamily::Serif);
    let second = &lines[1];
    assert_eq!(
        second
            .pieces
            .iter()
            .map(|p| (p.run, p.text.clone(), p.left, p.right))
            .collect::<Vec<_>>(),
        [(1, 5..9, 0, 64), (2, 0..4, 64, 128)]
    );
    assert_eq!(first.hit(0).map(|p| p.tag), Some(3));
    assert_eq!(first.hit(63).map(|p| p.tag), Some(3));
    assert_eq!(first.hit(64).map(|p| p.tag), Some(7));
    assert_eq!(first.hit(127).map(|p| p.tag), Some(7));
    assert_eq!(first.hit(128), None);
    assert_eq!(first.hit(-1), None);
    assert_eq!(second.hit(70).map(|p| p.run), Some(2));
}

#[test]
fn without_faces_a_prose_glyph_draws_as_a_cell_glyph() {
    for value in 1..=2 {
        let surface = Surface::new(40, 40, scale(value)).unwrap();
        let clip = surface.bounds();
        let cell = painted(
            surface,
            None,
            &[glyph(clip, (3, 5), 'g', style(TextFamily::Cell))],
        );
        for family in [TextFamily::Sans, TextFamily::Serif, TextFamily::Mono] {
            let prose = painted(surface, None, &[glyph(clip, (3, 5), 'g', style(family))]);
            assert_eq!(prose, cell, "{family:?} at scale {value}");
        }
    }
}

/// A page of prose lines on the cell, for the read-back.
struct Page {
    surface: Surface,
    lines: Vec<Line>,
}

impl Composition for Page {
    fn surface(&self) -> Surface {
        self.surface
    }
    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let step = (CELL_HEIGHT * self.surface.scale.value()) as i64;
        for (index, line) in self.lines.iter().enumerate() {
            draw(
                line,
                (0, index as i64 * step),
                self.surface.bounds(),
                damage,
                sink,
            );
        }
    }
}

#[test]
fn prose_on_the_cell_reads_back_as_its_text() {
    let surface = Surface::new(160, 64, scale(2)).unwrap();
    let mut metrics = Metrics::new(None, surface.scale);
    let runs = [
        run("the quick ", TextFamily::Sans, 0),
        run("brown fox", TextFamily::Mono, 1),
    ];
    let lines = wrap(&mut metrics, &runs, 160);
    let (_, _, text) = driven::text(&Page { surface, lines }).unwrap();
    assert_eq!(text, "the quick\nbrown fox");
}

// Over encoded fonts.

/// One prose line at (10, 4) in faces the raster draws it with.
struct Overhang {
    surface: Surface,
    line: Line,
}

impl Composition for Overhang {
    fn surface(&self) -> Surface {
        self.surface
    }
    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        draw(&self.line, (10, 4), self.surface.bounds(), damage, sink);
    }
}

#[test]
fn a_repaint_of_part_of_a_line_equals_the_whole_inside_its_damage() {
    let font = pinned().unwrap();
    let mut faces = faces();
    let surface = Surface::new(40, 30, scale(1)).unwrap();
    let mut metrics = Metrics::new(Some(&faces), surface.scale);
    let runs = [run("ij", TextFamily::Sans, 0)];
    let line = wrap(&mut metrics, &runs, 1000).remove(0);
    // 'j' 4.8 pixels left of its pen at 10 + 3: ink from 8.2.
    assert_eq!(line.glyphs[1].x, 3);
    let page = Overhang { surface, line };
    let mut paint = |damage: Rect| {
        let mut pixels = vec![GARBAGE; 40 * 30 * 4];
        {
            let mut raster = Raster::new(&mut pixels, &font, surface, 160)
                .unwrap()
                .with_prose(Some(&mut faces));
            raster.paint(&page, damage).unwrap();
        }
        pixels
    };
    let whole = paint(surface.bounds());
    // Ending at 'j's pen, the damage still holds the ink it hangs left.
    let damage = Rect {
        x: 0,
        y: 0,
        width: 13,
        height: 30,
    };
    let part = paint(damage);
    assert!(!untouched(&whole, surface, (8, 12)));
    for y in 0..30 {
        for x in 0..40 {
            let expected = if x < 13 {
                pixel(&whole, surface, (x, y))
            } else {
                u32::from_le_bytes([GARBAGE; 4])
            };
            assert_eq!(pixel(&part, surface, (x, y)), expected, "{x},{y}");
        }
    }
}

#[test]
fn metrics_measure_each_scalar_from_the_face_that_draws_it() {
    let loaded = faces_with_mono();
    let mut metrics = Metrics::new(Some(&loaded), scale(1));
    assert!(metrics.proportional());
    // Serif's 900 ascender over Sans's -300 descender: 1200 units, 19.2
    // pixels at 16 px/em, where each family alone is 17.6 high.
    assert_eq!(
        metrics.line(),
        LineBox {
            height: 19,
            baseline: 14
        }
    );
    let sans = TextFamily::Sans;
    // 200 units at 16 px/em is 3.2 pixels, 204.8 units of 1/64.
    assert_eq!(metrics.advance(sans, Style::Regular, 'i'), 205);
    assert_eq!(metrics.advance(sans, Style::Italic, 'm'), 819);
    assert_eq!(metrics.advance(sans, Style::Regular, ' '), 256);
    assert_eq!(metrics.advance(TextFamily::Serif, Style::Bold, 'i'), 307);
    assert_eq!(metrics.source(sans, Style::Regular, 'i'), Source::Family);
    // Sans lacks the rule: the monospace face's one advance.
    assert_eq!(metrics.source(sans, Style::Regular, '─'), Source::Mono);
    assert_eq!(metrics.advance(sans, Style::Regular, '─'), 614);
    // Neither has it: the bitmap face's cell.
    assert_eq!(metrics.source(sans, Style::Regular, 'é'), Source::Bitmap);
    assert_eq!(metrics.advance(sans, Style::Regular, 'é'), 8 * UNIT);
    // Mono starts at the monospace face.
    assert_eq!(
        metrics.source(TextFamily::Mono, Style::Regular, '0'),
        Source::Mono
    );
    assert_eq!(metrics.advance(TextFamily::Mono, Style::Regular, '0'), 614);
    assert_eq!(
        metrics.source(TextFamily::Mono, Style::Regular, 'i'),
        Source::Bitmap
    );
    // A cell glyph is the cell's whatever the faces.
    assert_eq!(
        metrics.source(TextFamily::Cell, Style::Regular, 'i'),
        Source::Cell
    );
    assert_eq!(
        metrics.advance(TextFamily::Cell, Style::Regular, 'i'),
        8 * UNIT
    );
    // Without the monospace face the rule is the bitmap face's.
    let mut bare = Metrics::new(Some(&faces()), scale(1));
    assert_eq!(bare.source(sans, Style::Regular, '─'), Source::Bitmap);
    assert_eq!(
        bare.advance(TextFamily::Mono, Style::Regular, '0'),
        8 * UNIT
    );
    // At scale two the size doubles.
    let mut double = Metrics::new(Some(&loaded), scale(2));
    assert_eq!(
        double.line(),
        LineBox {
            height: 38,
            baseline: 29
        }
    );
    assert_eq!(double.advance(sans, Style::Regular, 'i'), 410);
    assert_eq!(double.advance(sans, Style::Regular, 'é'), 16 * UNIT);
}

#[test]
fn pens_are_summed_in_subpixels_and_rounded_so_lines_do_not_drift() {
    let faces = faces();
    let mut metrics = Metrics::new(Some(&faces), scale(1));
    let text = "i".repeat(100);
    let runs = [run(&text, TextFamily::Sans, 0)];
    let lines = wrap(&mut metrics, &runs, u32::MAX);
    assert_eq!(lines.len(), 1);
    for (index, placed) in lines[0].glyphs.iter().enumerate() {
        assert_eq!(placed.x, ((index as u32 * 205) + 32) / 64, "glyph {index}");
    }
    // 100 × 3.203 pixels, not 100 × 3.
    assert_eq!(lines[0].width, 320);
    assert_eq!(metrics.width(TextFamily::Sans, Style::Regular, &text), 320);
}

#[test]
fn proportional_lines_wrap_by_pixel_width() {
    let faces = faces();
    let mut metrics = Metrics::new(Some(&faces), scale(1));
    let lay = |metrics: &mut Metrics, text: &str, width: u32| {
        let runs = [run(text, TextFamily::Sans, 0)];
        wrap(metrics, &runs, width)
            .iter()
            .map(|line| (shown(line, &runs), line.width))
            .collect::<Vec<_>>()
    };
    let own = |pairs: &[(&str, u32)]| -> Vec<(String, u32)> {
        pairs.iter().map(|(s, w)| (s.to_string(), *w)).collect()
    };
    // "mm ii" ends at 36 pixels, "mm ii mm" at 66.
    assert_eq!(lay(&mut metrics, "mm ii mm", 66), own(&[("mm ii mm", 66)]));
    assert_eq!(
        lay(&mut metrics, "mm ii mm", 40),
        own(&[("mm ii", 36), ("mm", 26)])
    );
    assert_eq!(
        lay(&mut metrics, "mm ii mm", 36),
        own(&[("mm ii", 36), ("mm", 26)])
    );
    assert_eq!(
        lay(&mut metrics, "mm ii mm", 35),
        own(&[("mm", 26), ("ii", 6), ("mm", 26)])
    );
    // A word wider than the line breaks inside it.
    assert_eq!(
        lay(&mut metrics, "mmmmmm", 30),
        own(&[("mm", 26), ("mm", 26), ("mm", 26)])
    );
    // Two families on one line, each piece its own extent.
    let runs = [
        run("mm ", TextFamily::Sans, 7),
        run("ii", TextFamily::Serif, 9),
    ];
    let line = &wrap(&mut metrics, &runs, 1000)[0];
    let extents: Vec<(usize, u32, u32)> = line
        .pieces
        .iter()
        .map(|p| (p.tag, p.left, p.right))
        .collect();
    assert_eq!(extents, [(7, 0, 30), (9, 30, 39)]);
    assert_eq!(line.hit(29).map(|p| p.tag), Some(7));
    assert_eq!(line.hit(30).map(|p| p.tag), Some(9));
    assert_eq!(line.hit(39), None);
}

#[test]
fn a_prose_glyph_draws_at_its_pen_and_the_line_baseline_clipped_by_its_draw_not_a_cell() {
    let surface = Surface::new(40, 30, scale(1)).unwrap();
    let mut faces = faces();
    let pixels = painted(
        surface,
        Some(&mut faces),
        &[glyph(
            surface.bounds(),
            (3, 2),
            'm',
            style(TextFamily::Sans),
        )],
    );
    // 'm' is 11.2 by 8 pixels standing on the baseline at 2 + 14.
    for (x, y) in [(3, 8), (13, 15), (12, 10)] {
        assert_eq!(pixel(&pixels, surface, (x, y)), INK | 0xff000000, "{x},{y}");
    }
    let edge = pixel(&pixels, surface, (14, 10));
    assert_ne!(edge, INK | 0xff000000);
    assert!(
        !untouched(&pixels, surface, (14, 10)),
        "the fifth of a pixel"
    );
    for at in [(15, 10), (3, 7), (3, 16), (2, 10)] {
        assert!(untouched(&pixels, surface, at), "{at:?}");
    }
    // Clipped to the draw's clip, which is all that bounds it.
    let clip = Rect {
        x: 0,
        y: 0,
        width: 10,
        height: 30,
    };
    let clipped = painted(
        surface,
        Some(&mut faces),
        &[glyph(clip, (3, 2), 'm', style(TextFamily::Sans))],
    );
    for y in 0..30 {
        for x in 0..40 {
            if x < 10 {
                assert_eq!(
                    pixel(&clipped, surface, (x, y)),
                    pixel(&pixels, surface, (x, y))
                );
            } else {
                assert!(untouched(&clipped, surface, (x, y)));
            }
        }
    }
    // Nor is a glyph taller and wider than a cell cut to one.
    let tall = painted(
        surface,
        Some(&mut faces),
        &[glyph(
            surface.bounds(),
            (3, 2),
            'T',
            style(TextFamily::Sans),
        )],
    );
    // 16 pixels square from 12.8 above the baseline: whole from row 4
    // through 18, past the 16 rows and 8 columns of a cell at (3, 2).
    assert_eq!(pixel(&tall, surface, (10, 4)), INK | 0xff000000);
    assert_eq!(pixel(&tall, surface, (10, 18)), INK | 0xff000000);
    assert_eq!(pixel(&tall, surface, (17, 10)), INK | 0xff000000);
    assert!(untouched(&tall, surface, (10, 20)));
}

#[test]
fn a_scalar_the_family_lacks_draws_from_the_monospace_face_then_the_bitmap_face() {
    for value in 1..=2 {
        let surface = Surface::new(60, 60, scale(value)).unwrap();
        let clip = surface.bounds();
        let mut with_mono = faces_with_mono();
        let rule = painted(
            surface,
            Some(&mut with_mono),
            &[glyph(clip, (4, 6), '─', style(TextFamily::Sans))],
        );
        let in_mono = painted(
            surface,
            Some(&mut with_mono),
            &[glyph(clip, (4, 6), '─', style(TextFamily::Mono))],
        );
        assert_eq!(rule, in_mono);
        assert!(rule.chunks(4).any(|p| p != [GARBAGE; 4]));
        // Neither face has 'é': the bitmap glyph, its baseline on the
        // line's.
        let baseline = Metrics::new(Some(&with_mono), surface.scale)
            .line()
            .baseline;
        let top = 6 + i64::from(baseline) - (CELL_BASELINE * usize::from(value)) as i64;
        let bitmap = painted(
            surface,
            None,
            &[glyph(clip, (4, top), 'é', style(TextFamily::Cell))],
        );
        for family in [TextFamily::Sans, TextFamily::Serif, TextFamily::Mono] {
            let fallen = painted(
                surface,
                Some(&mut with_mono),
                &[glyph(clip, (4, 6), 'é', style(family))],
            );
            assert_eq!(fallen, bitmap, "{family:?} at scale {value}");
        }
        // Without the monospace face the rule is the bitmap face's too.
        let mut bare = faces();
        let fallen = painted(
            surface,
            Some(&mut bare),
            &[glyph(clip, (4, 6), '─', style(TextFamily::Sans))],
        );
        let bitmap = painted(
            surface,
            None,
            &[glyph(clip, (4, top), '─', style(TextFamily::Cell))],
        );
        assert_eq!(fallen, bitmap);
    }
}

#[test]
fn a_cell_glyph_is_unchanged_by_the_prose_faces() {
    let surface = Surface::new(40, 40, scale(2)).unwrap();
    let draws = [glyph(
        surface.bounds(),
        (3, 5),
        'm',
        style(TextFamily::Cell),
    )];
    assert_eq!(
        painted(surface, Some(&mut faces_with_mono()), &draws),
        painted(surface, None, &draws)
    );
}

#[test]
fn a_raster_measures_with_the_faces_it_draws_with() {
    let font = pinned().unwrap();
    let mut faces = faces_with_mono();
    for value in [1, 2, 1] {
        let surface = Surface::new(20, 20, scale(value)).unwrap();
        let mut pixels = vec![0; 20 * 20 * 4];
        let mut raster = Raster::new(&mut pixels, &font, surface, 80)
            .unwrap()
            .with_prose(Some(&mut faces));
        let measured = raster.metrics();
        let mut direct = Metrics::new(Some(&faces_with_mono()), surface.scale);
        assert_eq!(measured.line(), direct.line());
        for scalar in ['i', 'm', '─', 'é'] {
            assert_eq!(
                measured.advance(TextFamily::Sans, Style::Regular, scalar),
                direct.advance(TextFamily::Sans, Style::Regular, scalar)
            );
        }
        let mut bare = Raster::new(&mut pixels, &font, surface, 80).unwrap();
        assert!(!bare.metrics().proportional());
        assert_eq!(bare.metrics().scale(), surface.scale);
    }
}

#[test]
fn faces_refuse_a_style_by_family_and_name() {
    let mut serif = four(serif());
    serif[2] = b"not a font".to_vec();
    assert_eq!(
        Faces::new(four(sans()), serif).unwrap_err(),
        Refused {
            family: TextFamily::Serif,
            style: Style::Italic,
            error: Error::Malformed("sfnt version"),
        }
    );
}

#[test]
fn the_loader_reads_both_families_and_says_what_is_missing() {
    let temp = temp_dir::TempDir::new("td-ui-prose").unwrap();
    let dir = temp.path();
    for (family, bytes) in [(Family::SANS, sans()), (Family::SERIF, serif())] {
        for name in [
            family.regular,
            family.bold,
            family.italic,
            family.bold_italic,
        ] {
            fs::write(dir.join(name), &bytes).unwrap();
        }
    }
    let places = [td_ui::face_file::Place {
        dir: dir.to_path_buf(),
        depth: 0,
    }];
    let faces = prose_in(&places).unwrap();
    assert!(!faces.has_mono());
    assert!(Metrics::new(Some(&faces), scale(1)).proportional());
    let typeface = mono();
    let with_mono = prose_in_or_note(&places, "test", None, Some(&typeface)).unwrap();
    assert!(with_mono.has_mono());
    assert!(prose_in_or_note(&places, "test", Some(OsStr::new("bitmap")), None).is_none());
    // A refused style names its file.
    fs::write(dir.join(Family::SERIF.bold), b"no").unwrap();
    let why = prose_from(dir, dir).unwrap_err();
    assert!(
        why.starts_with(&dir.join(Family::SERIF.bold).display().to_string()),
        "{why}"
    );
    // A missing style names its path; a missing family the places looked
    // in, and the program goes on without the faces.
    fs::remove_file(dir.join(Family::SANS.italic)).unwrap();
    let why = prose_from(dir, dir).unwrap_err();
    assert!(
        why.starts_with(&dir.join(Family::SANS.italic).display().to_string()),
        "{why}"
    );
    fs::remove_file(dir.join(Family::SANS.regular)).unwrap();
    let why = prose_in(&places).unwrap_err();
    assert!(why.contains(Family::SANS.regular), "{why}");
    assert!(prose_in_or_note(&places, "test", None, None).is_none());
}

// Over the pinned faces. These read only the places `./install-fonts`
// and the image put them, never a host's packaged faces, and fail when
// the faces are not there; they are ignored unless asked for:
// `XDG_DATA_HOME=<an install> cargo test --test prose -- --ignored`.

/// The pinned faces' directories for `family`: the image's and the XDG
/// data home's, not the host font roots.
fn pinned_places(family: Family) -> Vec<td_ui::face_file::Place> {
    family
        .host_places()
        .into_iter()
        .filter(|place| place.depth == 0)
        .collect()
}

/// The pinned prose faces, and the monospace face's four styles.
fn installed() -> (Faces, Typeface) {
    let faces = prose_in(&pinned_places(Family::SANS))
        .unwrap_or_else(|why| panic!("the pinned prose faces are not installed: {why}"));
    let mono = load_styled_in(&pinned_places(Family::MONO))
        .unwrap_or_else(|why| panic!("the pinned monospace face is not installed: {why}"));
    (faces, mono)
}

#[test]
#[ignore = "needs the pinned faces: ./install-fonts, or XDG_DATA_HOME at an install"]
fn pinned_faces_measure_real_advances() {
    let (faces, _) = installed();
    for value in 1..=4 {
        let mut metrics = Metrics::new(Some(&faces), scale(value));
        for family in [TextFamily::Sans, TextFamily::Serif] {
            for style in [
                Style::Regular,
                Style::Bold,
                Style::Italic,
                Style::BoldItalic,
            ] {
                let i = metrics.advance(family, style, 'i');
                let m = metrics.advance(family, style, 'm');
                assert!(i < m / 2, "{family:?} {style:?}: i {i}, m {m}");
                assert_eq!(metrics.source(family, style, 'm'), Source::Family);
            }
        }
        // Noto's 1069 ascender and -293 descender: 22 and 17 pixels at
        // 16 px/em, each scale's rounded on its own.
        let size = f64::from(SIZE) * f64::from(value);
        assert_eq!(
            metrics.line(),
            LineBox {
                height: (1362.0 * size / 1000.0).round() as u32,
                baseline: (1069.0 * size / 1000.0).round() as u32,
            }
        );
    }
    assert_eq!(
        Metrics::new(Some(&faces), scale(1)).line(),
        LineBox {
            height: 22,
            baseline: 17
        }
    );
}

const PARAGRAPH: &str = "Prose reads better in a face whose glyphs take their \
own widths, and a browser, which shows documents, needs one. This lays \
styled runs into lines no wider than a width in pixels, breaking after a \
space and inside a word only when the word alone is wider than the line.";

#[test]
#[ignore = "needs the pinned faces: ./install-fonts, or XDG_DATA_HOME at an install"]
fn pinned_prose_wraps_by_pixel_width() {
    let (faces, _) = installed();
    for (value, width) in [(1, 300), (2, 600), (1, 173)] {
        let mut metrics = Metrics::new(Some(&faces), scale(value));
        for family in [TextFamily::Sans, TextFamily::Serif] {
            let runs = [run(PARAGRAPH, family, 0)];
            let lines = wrap(&mut metrics, &runs, width);
            assert!(lines.len() > 2);
            let texts: Vec<String> = lines.iter().map(|line| shown(line, &runs)).collect();
            assert_eq!(texts.join(" "), PARAGRAPH, "no word broken");
            for (index, line) in lines.iter().enumerate() {
                assert!(
                    line.width <= width,
                    "{family:?} line {index} {}",
                    line.width
                );
                assert_eq!(
                    line.width,
                    metrics.width(family, Style::Regular, &texts[index])
                );
                // Greedy: the next line's first word would not have fit.
                if let Some(next) = texts.get(index + 1) {
                    let word = next.split(' ').next().unwrap();
                    let longer = format!("{} {word}", texts[index]);
                    assert!(
                        metrics.width(family, Style::Regular, &longer) > width,
                        "{longer}"
                    );
                }
            }
        }
    }
}

#[test]
#[ignore = "needs the pinned faces: ./install-fonts, or XDG_DATA_HOME at an install"]
fn pinned_pens_do_not_drift_over_a_long_line() {
    let (faces, _) = installed();
    let text = "The quick brown fox jumps over the lazy dog. ".repeat(50);
    let text = text.trim_end();
    let mut metrics = Metrics::new(Some(&faces), scale(1));
    let runs = [run(text, TextFamily::Serif, 0)];
    let lines = wrap(&mut metrics, &runs, u32::MAX);
    assert_eq!(lines.len(), 1);
    let mut pen = 0u64;
    for (placed, scalar) in lines[0].glyphs.iter().zip(text.chars()) {
        assert_eq!(u64::from(placed.x), (pen + 32) / 64);
        pen += u64::from(metrics.advance(TextFamily::Serif, Style::Regular, scalar));
    }
    assert_eq!(u64::from(lines[0].width), (pen + 32) / 64);
    assert_eq!(
        lines[0].width,
        metrics.width(TextFamily::Serif, Style::Regular, text)
    );
}

#[test]
#[ignore = "needs the pinned faces: ./install-fonts, or XDG_DATA_HOME at an install"]
fn pinned_fallback_draws_from_the_monospace_face_then_unifont() {
    let (faces, mono) = installed();
    let surface = Surface::new(80, 60, scale(1)).unwrap();
    let clip = surface.bounds();
    // A Powerline glyph from the private use area, and a box-drawing rule.
    for scalar in ['\u{e0a0}', '─'] {
        let mut bare = faces.clone();
        let mut metrics = Metrics::new(Some(&bare), surface.scale);
        assert_eq!(
            metrics.source(TextFamily::Sans, Style::Regular, scalar),
            Source::Bitmap
        );
        let top = 4 + i64::from(metrics.line().baseline) - CELL_BASELINE as i64;
        assert_eq!(
            painted(
                surface,
                Some(&mut bare),
                &[glyph(clip, (4, 4), scalar, style(TextFamily::Sans))]
            ),
            painted(
                surface,
                None,
                &[glyph(clip, (4, top), scalar, style(TextFamily::Cell))]
            )
        );
        let mut with_mono = faces.clone().with_mono(&mono).unwrap();
        let mut metrics = Metrics::new(Some(&with_mono), surface.scale);
        assert_eq!(
            metrics.source(TextFamily::Sans, Style::Regular, scalar),
            Source::Mono
        );
        // JetBrains Mono's 600-unit advance at 16 px/em.
        assert_eq!(metrics.advance(TextFamily::Serif, Style::Bold, scalar), 614);
        let fallen = painted(
            surface,
            Some(&mut with_mono),
            &[glyph(clip, (4, 4), scalar, style(TextFamily::Sans))],
        );
        let in_mono = painted(
            surface,
            Some(&mut with_mono),
            &[glyph(clip, (4, 4), scalar, style(TextFamily::Mono))],
        );
        assert_eq!(fallen, in_mono);
        assert!(fallen.chunks(4).any(|p| p != [GARBAGE; 4]));
    }
    // A scalar no face has draws Unifont's replacement.
    let mut with_mono = faces.clone().with_mono(&mono).unwrap();
    let mut metrics = Metrics::new(Some(&with_mono), surface.scale);
    assert_eq!(
        metrics.source(TextFamily::Sans, Style::Regular, '\u{4e00}'),
        Source::Bitmap
    );
    let top = 4 + i64::from(metrics.line().baseline) - CELL_BASELINE as i64;
    assert_eq!(
        painted(
            surface,
            Some(&mut with_mono),
            &[glyph(clip, (4, 4), '\u{4e00}', style(TextFamily::Sans))]
        ),
        painted(
            surface,
            None,
            &[glyph(clip, (4, top), '\u{4e00}', style(TextFamily::Cell))]
        )
    );
}

#[test]
#[ignore = "needs the pinned faces: ./install-fonts, or XDG_DATA_HOME at an install"]
fn pinned_glyphs_clip_to_their_bounds_not_to_a_cell() {
    let (mut faces, _) = installed();
    let surface = Surface::new(40, 30, scale(1)).unwrap();
    let whole = painted(
        surface,
        Some(&mut faces),
        &[glyph(
            surface.bounds(),
            (0, 0),
            'W',
            style(TextFamily::Sans),
        )],
    );
    // Noto Sans's W is wider than a cell.
    assert!((CELL_WIDTH..40).any(|x| (0..30).any(|y| !untouched(&whole, surface, (x, y)))));
    let clip = Rect {
        x: 0,
        y: 0,
        width: 10,
        height: 30,
    };
    let clipped = painted(
        surface,
        Some(&mut faces),
        &[glyph(clip, (0, 0), 'W', style(TextFamily::Sans))],
    );
    for y in 0..30 {
        for x in 0..40 {
            if x < 10 {
                assert_eq!(
                    pixel(&clipped, surface, (x, y)),
                    pixel(&whole, surface, (x, y))
                );
            } else {
                assert!(untouched(&clipped, surface, (x, y)));
            }
        }
    }
}

// Existing geometry oracles use ample admission; refusal tests call the API directly.
fn wrap(metrics: &mut Metrics, runs: &[Run], width: u32) -> Vec<Line> {
    td_ui::prose::wrap(
        metrics,
        runs,
        width,
        &td_alloc::Budget::new(64 << 20).unwrap(),
    )
    .unwrap()
    .into_iter()
    .collect()
}
fn wrap_at_most(metrics: &mut Metrics, runs: &[Run], width: u32, most: usize) -> Vec<Line> {
    td_ui::prose::wrap_at_most(
        metrics,
        runs,
        width,
        most,
        &td_alloc::Budget::new(64 << 20).unwrap(),
    )
    .unwrap()
    .into_iter()
    .collect()
}

#[test]
fn every_wrapping_allocation_may_refuse_and_owners_return_their_credit() {
    use td_alloc::{with_reservation_limit, Budget};
    let budget = Budget::new(2 << 20).unwrap();
    let base = budget.used();
    let runs = [
        run("a long word and\n", TextFamily::Sans, 3),
        run("miiii  mii", TextFamily::Serif, 4),
    ];
    let expected = wrap(&mut Metrics::new(None, scale(1)), &runs, 40);
    let mut succeeded = false;
    for n in 0..256 {
        let result = with_reservation_limit(n, || {
            td_ui::prose::wrap(&mut Metrics::new(None, scale(1)), &runs, 40, &budget)
        });
        if let Ok(lines) = result {
            assert_eq!(lines.as_slice(), expected.as_slice());
            assert!(budget.used() > base);
            // Moving and consuming iteration retain admission until the backing drops.
            let mut moved = lines.into_iter();
            let first = moved.next().unwrap();
            drop(moved);
            assert!(budget.used() > base);
            drop(first);
            succeeded = true;
        }
        assert_eq!(budget.used(), base, "allowance {n}");
        if succeeded {
            break;
        }
    }
    assert!(succeeded);
    let blocker = budget.charge(budget.limit() - budget.used()).unwrap();
    assert!(td_ui::prose::wrap(&mut Metrics::new(None, scale(1)), &runs, 40, &budget).is_err());
    drop(blocker);
    assert!(td_ui::prose::wrap(&mut Metrics::new(None, scale(1)), &runs, 40, &budget).is_ok());
}

#[test]
fn cache_refusal_is_exact_and_clones_keep_the_same_budget() {
    use td_alloc::{with_reservation_limit, Budget};
    let faces = faces_with_mono();
    let budget = Budget::new(1 << 20).unwrap();
    let base = budget.used();
    let text = "mi0 Tj\u{301}─🦀 ";
    let mut expected = Metrics::new(Some(&faces), scale(2));
    for allowance in 0..10 {
        let mut metrics = Metrics::new_in(Some(&faces), scale(2), &budget);
        with_reservation_limit(allowance, || {
            for _ in 0..3 {
                for family in [TextFamily::Sans, TextFamily::Serif, TextFamily::Mono] {
                    for scalar in text.chars() {
                        assert_eq!(
                            metrics.advance(family, Style::Italic, scalar),
                            expected.advance(family, Style::Italic, scalar)
                        );
                        assert_eq!(
                            metrics.source(family, Style::Italic, scalar),
                            expected.source(family, Style::Italic, scalar)
                        );
                    }
                }
            }
        });
        let mut copied = metrics.clone();
        drop(metrics);
        assert_eq!(
            copied.width(TextFamily::Sans, Style::Regular, text),
            expected.width(TextFamily::Sans, Style::Regular, text)
        );
        drop(copied);
        assert_eq!(budget.used(), base);
    }
    // Actual shared cap refusal also uses the uncached result.
    let blocker = budget.charge(budget.limit() - budget.used()).unwrap();
    let mut uncached = Metrics::new_in(Some(&faces), scale(2), &budget);
    assert_eq!(
        uncached.width(TextFamily::Sans, Style::Regular, text),
        expected.width(TextFamily::Sans, Style::Regular, text)
    );
    assert_eq!((budget.limit() - budget.used()), 0);
    drop((uncached, blocker));
    assert_eq!(budget.used(), base);
}
