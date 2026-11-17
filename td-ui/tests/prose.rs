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
//! face; prose at a size: the bound, advances and lines scaled with it,
//! ink inside the advance and an em, the prose size named drawing as the
//! default, the bitmap face at the size's scale, wrapping by sized runs,
//! the atlas pages' cap, eviction and remaking, a frame past the cap, and
//! measuring and drawing faces made apart agreeing; the prose faces'
//! loader over a directory the test writes; and,
//! ignored unless asked for, the same oracles over the pinned faces
//! where `./install-fonts` or the image puts them.

mod fonts;
// The shared temporary-tree guard (AGENTS.md "Parallel work"), compiled
// in because td-ui, the leaf, names no dependency.
#[path = "../../td-fs/src/temp_dir.rs"]
mod temp_dir;

use std::ffi::OsStr;
use std::fs;

use fonts::{encoded, encoded_per_em, encoded_reaching, rectangle, Glyph};
use td_ui::atlas::{Style, PAGE_HEIGHT, PAGE_WIDTH};
use td_ui::driven;
use td_ui::face_file::Family;
use td_ui::font::pinned;
use td_ui::pinned_face::{load_styled_in, prose_from, prose_in, prose_in_or_note};
use td_ui::prose::{
    draw, overhang, Faces, Line, LineBox, Metrics, Reach, Refused, Run, Size, Source, ATLAS,
    CELL_BASELINE, MAX_SIZE, MIN_SIZE, OVERHANG, PAGES, PAGE_KEYS, SIZE, UNIT,
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

// Prose at a size.

/// `units` of a 1000-unit em at `pixels` per em, in `UNIT`s, rounded.
fn at(units: i64, pixels: i64) -> u32 {
    ((units * pixels * 64 * 2 + 1000) / 2000) as u32
}

/// Glyphs in the style of `family` at `size`.
fn sized(family: TextFamily, size: Size) -> GlyphStyle {
    style(family).with_size(size)
}

/// The columns and rows of the pixels a paint inked, inclusive.
fn ink(pixels: &[u8], surface: Surface) -> Option<((usize, usize), (usize, usize))> {
    let mut found: Option<((usize, usize), (usize, usize))> = None;
    for y in 0..surface.height {
        for x in 0..surface.width {
            if !untouched(pixels, surface, (x, y)) {
                found = Some(match found {
                    None => ((x, x), (y, y)),
                    Some(((l, r), (t, b))) => ((l.min(x), r.max(x)), (t.min(y), b.max(y))),
                });
            }
        }
    }
    found
}

#[test]
fn a_size_is_refused_or_clamped_to_its_bound_by_name() {
    // Every size a face may be covered at, the prose size's at every
    // scale among them.
    const {
        assert!(MIN_SIZE >= td_ui::face::MIN_PIXELS_PER_EM);
        assert!(MAX_SIZE <= td_ui::face::MAX_PIXELS_PER_EM);
        assert!(MIN_SIZE <= SIZE && SIZE * 4 <= MAX_SIZE);
    }
    for refused in [
        0,
        1,
        u32::from(MIN_SIZE) - 1,
        u32::from(MAX_SIZE) + 1,
        u32::MAX,
    ] {
        assert_eq!(
            Size::new(refused),
            Err(Error::Limit("prose size")),
            "{refused}"
        );
    }
    for admitted in [MIN_SIZE, 17, MAX_SIZE] {
        let size = Size::new(u32::from(admitted)).unwrap();
        assert!(!size.is_default());
        for value in 1..=4 {
            assert_eq!(size.pixels(scale(value)), admitted);
        }
    }
    assert_eq!(Size::clamped(0), Size::new(u32::from(MIN_SIZE)).unwrap());
    assert_eq!(Size::clamped(17), Size::new(17).unwrap());
    assert_eq!(
        Size::clamped(u32::MAX),
        Size::new(u32::from(MAX_SIZE)).unwrap()
    );
    // The default is the prose size at the scale, and every glyph's.
    assert!(Size::DEFAULT.is_default());
    assert_eq!(GlyphStyle::regular(INK, PAPER).size, Size::DEFAULT);
    for value in 1..=4 {
        assert_eq!(Size::DEFAULT.pixels(scale(value)), SIZE * u16::from(value));
        assert_eq!(
            overhang(Size::DEFAULT, scale(value)),
            u32::from(SIZE) * u32::from(value)
        );
    }
    // An em at each size; the constant is the default's at scale four.
    assert_eq!(overhang(Size::DEFAULT, scale(4)), OVERHANG);
    assert_eq!(overhang(Size::new(150).unwrap(), scale(1)), 150);
}

#[test]
fn metrics_scale_each_advance_and_the_line_with_the_size() {
    let loaded = faces_with_mono();
    let (sans, serif, mono) = (TextFamily::Sans, TextFamily::Serif, TextFamily::Mono);
    for value in 1..=3 {
        let mut metrics = Metrics::new(Some(&loaded), scale(value));
        for pixels in [MIN_SIZE, 10, 24, 33, 100, MAX_SIZE] {
            let size = Size::new(u32::from(pixels)).unwrap();
            let p = i64::from(pixels);
            // Unhinted: each face's units at the size, rounded to a UNIT.
            assert_eq!(
                metrics.advance_at(size, sans, Style::Regular, 'i'),
                at(200, p)
            );
            assert_eq!(
                metrics.advance_at(size, sans, Style::Italic, 'm'),
                at(800, p)
            );
            assert_eq!(
                metrics.advance_at(size, serif, Style::Bold, 'i'),
                at(300, p)
            );
            assert_eq!(
                metrics.advance_at(size, sans, Style::Regular, '─'),
                at(600, p)
            );
            assert_eq!(
                metrics.advance_at(size, mono, Style::Regular, '0'),
                at(600, p)
            );
            // The bitmap face at its whole scale nearest the size.
            let bitmap = (usize::from(pixels) + 8) / 16;
            assert_eq!(
                metrics.advance_at(size, sans, Style::Regular, 'é'),
                (8 * bitmap.max(1)) as u32 * UNIT
            );
            // A cell glyph is the cell's at every size.
            assert_eq!(
                metrics.advance_at(size, TextFamily::Cell, Style::Regular, 'i'),
                8 * u32::from(value) * UNIT
            );
            // Which face draws a scalar does not move with the size.
            assert_eq!(metrics.source(sans, Style::Regular, '─'), Source::Mono);
            assert_eq!(
                metrics.width_at(size, sans, Style::Regular, "mi"),
                (at(800, p) + at(200, p) + 32) / 64
            );
            // Serif's 900 ascender over Sans's -300 descender, each in
            // UNITs at the size, rounded once.
            let (ascent, descent) = (at(900, p), at(300, p));
            assert_eq!(
                metrics.line_at(size),
                LineBox {
                    height: ((ascent + descent + 32) / 64).max(1),
                    baseline: (ascent + 32) / 64,
                }
            );
        }
        // The prose size named is the default's measurement.
        let named = Size::new(u32::from(SIZE) * u32::from(value)).unwrap();
        assert_eq!(metrics.line_at(named), metrics.line());
        assert_eq!(metrics.line_at(Size::DEFAULT), metrics.line());
        for scalar in ['i', 'm', '─', 'é', '0'] {
            for family in [sans, serif, mono] {
                assert_eq!(
                    metrics.advance_at(named, family, Style::Bold, scalar),
                    metrics.advance(family, Style::Bold, scalar)
                );
            }
        }
    }
    // Without faces every size is the cell at the scale.
    let mut cell = Metrics::new(None, scale(2));
    let size = Size::new(100).unwrap();
    assert_eq!(cell.line_at(size), cell.line());
    assert_eq!(cell.advance_at(size, sans, Style::Regular, 'm'), 16 * UNIT);
}

#[test]
fn a_glyph_at_a_size_inks_inside_its_advance_and_an_em_of_overhang() {
    let surface = Surface::new(420, 300, scale(1)).unwrap();
    let mut faces = faces();
    let mut metrics = Metrics::new(Some(&faces), surface.scale);
    let pen = 200;
    for pixels in [MIN_SIZE, 13, 40, 100, MAX_SIZE] {
        let size = Size::new(u32::from(pixels)).unwrap();
        let line = metrics.line_at(size);
        let p = usize::from(pixels);
        // 'T' is a full em from 200 units below the baseline to 800 above,
        // its advance the em: its ink is exactly its advance wide.
        let drawn = painted(
            surface,
            Some(&mut faces),
            &[glyph(
                surface.bounds(),
                (pen, 4),
                'T',
                sized(TextFamily::Sans, size),
            )],
        );
        let advance = metrics.advance_at(size, TextFamily::Sans, Style::Regular, 'T');
        assert_eq!(advance, u32::from(pixels) * UNIT);
        let ((left, right), (top, bottom)) = ink(&drawn, surface).unwrap();
        assert_eq!(
            (left, right),
            (pen as usize, pen as usize + p - 1),
            "{pixels}"
        );
        let baseline = 4 + line.baseline as usize;
        assert!(
            top >= baseline - (p * 8).div_ceil(10) && top >= 4,
            "{pixels}"
        );
        assert!(bottom < 4 + line.height as usize, "{pixels}");
        // 'j' hangs 300 units left of its pen: inside an em of overhang.
        let drawn = painted(
            surface,
            Some(&mut faces),
            &[glyph(
                surface.bounds(),
                (pen, 4),
                'j',
                sized(TextFamily::Sans, size),
            )],
        );
        let ((left, right), _) = ink(&drawn, surface).unwrap();
        let reach = overhang(size, surface.scale) as usize;
        assert_eq!(left, pen as usize - (p * 3).div_ceil(10), "{pixels}");
        assert!(left >= pen as usize - reach);
        let advance = metrics.advance_at(size, TextFamily::Sans, Style::Regular, 'j');
        assert!(
            right < pen as usize + (advance as usize).div_ceil(64),
            "{pixels}"
        );
    }
}

#[test]
fn the_prose_size_named_draws_pixel_for_pixel_as_the_default() {
    let text = "mi0Tj─é ";
    for value in 1..=4 {
        let surface = Surface::new(160, 60, scale(value)).unwrap();
        let named = Size::new(u32::from(SIZE) * u32::from(value)).unwrap();
        let draws = |size: Size| -> Vec<Draw> {
            let mut out = Vec::new();
            for (row, family) in [TextFamily::Sans, TextFamily::Serif, TextFamily::Mono]
                .into_iter()
                .enumerate()
            {
                for (column, scalar) in text.chars().enumerate() {
                    let style = sized(family, size).bold();
                    out.push(glyph(
                        surface.bounds(),
                        (column as i64 * 17, row as i64 * 19),
                        scalar,
                        style,
                    ));
                }
            }
            out
        };
        let mut faces = faces_with_mono();
        let default = painted(surface, Some(&mut faces), &draws(Size::DEFAULT));
        assert_eq!(faces.pages_held(), 0);
        let mut faces = faces_with_mono();
        assert_eq!(
            painted(surface, Some(&mut faces), &draws(named)),
            default,
            "scale {value}"
        );
        // Drawn from the scale's own faces: no page of another size.
        assert_eq!((faces.pages_held(), faces.pages_made()), (0, 0));
    }
}

#[test]
fn a_scalar_no_face_has_draws_the_bitmap_face_at_its_scale_for_the_size() {
    let surface = Surface::new(260, 260, scale(1)).unwrap();
    let clip = surface.bounds();
    let mut faces = faces_with_mono();
    let metrics = Metrics::new(Some(&faces), surface.scale);
    for (pixels, bitmap) in [(MIN_SIZE, 1), (23, 1), (24, 2), (40, 3), (MAX_SIZE, 12)] {
        let size = Size::new(u32::from(pixels)).unwrap();
        let fallen = painted(
            surface,
            Some(&mut faces),
            &[glyph(clip, (4, 6), 'é', sized(TextFamily::Serif, size))],
        );
        // The bitmap glyph at that whole scale, its baseline on the line's.
        let baseline = i64::from(metrics.line_at(size).baseline);
        let top = 6 + baseline - (CELL_BASELINE * bitmap) as i64;
        let at_scale = if bitmap <= 4 {
            Surface::new(260, 260, scale(bitmap as u8)).unwrap()
        } else {
            // Past `Scale`'s four, by the glyph's own extent instead.
            let ((left, right), (top_row, bottom)) = ink(&fallen, surface).unwrap();
            assert!(left >= 4 && right < 4 + 8 * bitmap);
            assert!(top_row >= top as usize && bottom < top as usize + 16 * bitmap);
            continue;
        };
        let expected = painted(
            at_scale,
            None,
            &[glyph(clip, (4, top), 'é', style(TextFamily::Cell))],
        );
        assert_eq!(fallen, expected, "{pixels}");
    }
}

#[test]
fn wrap_measures_each_run_at_its_size_and_draw_reaches_its_largest_em() {
    let faces = faces();
    let mut metrics = Metrics::new(Some(&faces), scale(1));
    let large = Size::new(32).unwrap();
    let runs = [
        Run {
            text: "mm",
            style: sized(TextFamily::Sans, large),
            tag: 1,
        },
        run("ii", TextFamily::Sans, 2),
    ];
    let line = &wrap(&mut metrics, &runs, 1000)[0];
    // 'm' at 32 px/em is 1638 UNITs, 'i' at 16 is 205.
    let extents: Vec<(usize, u32, u32)> = line
        .pieces
        .iter()
        .map(|p| (p.tag, p.left, p.right))
        .collect();
    assert_eq!(extents, [(1, 0, 51), (2, 51, 58)]);
    assert_eq!(line.pieces[0].style.size, large);
    // Breaking by the sized advances: two 'm's at 32 are 51 pixels.
    let lines = wrap(&mut metrics, &runs[..1], 50);
    assert_eq!(lines.len(), 2);
    // A line at 150 px/em streams pens up to 150 past its clip, not 64.
    let text = "i".repeat(60);
    let runs = [Run {
        text: &text,
        style: sized(TextFamily::Sans, Size::new(150).unwrap()),
        tag: 0,
    }];
    let line = &wrap(&mut metrics, &runs, u32::MAX)[0];
    let bounds = Rect {
        x: 0,
        y: 0,
        width: 20,
        height: 200,
    };
    let mut draws = Vec::new();
    draw(line, (0, 0), bounds, bounds, &mut |d| draws.push(d));
    // 'i' is 30 pixels: pens 0 through 150 are before 20 + 150.
    assert_eq!(draws.len(), 6);
}

/// A frame of each `(family, size)` drawn as five glyphs a row.
fn frame(combos: &[(TextFamily, u16)]) -> Vec<Draw> {
    let clip = Rect {
        x: 0,
        y: 0,
        width: 400,
        height: 400,
    };
    let mut draws = Vec::new();
    for (row, &(family, pixels)) in combos.iter().enumerate() {
        let size = Size::new(u32::from(pixels)).unwrap();
        for (column, scalar) in "mi0Tj".chars().enumerate() {
            let style = if column % 2 == 0 {
                sized(family, size)
            } else {
                sized(family, size).italic()
            };
            draws.push(glyph(
                clip,
                (column as i64 * 45 + 30, row as i64 * 31),
                scalar,
                style,
            ));
        }
    }
    draws
}

#[test]
fn the_pages_hold_their_cap_over_many_sizes_and_families() {
    let surface = Surface::new(400, 400, scale(1)).unwrap();
    let mut faces = faces_with_mono();
    let mut made = 0;
    for pixels in 20..=40 {
        for family in [TextFamily::Sans, TextFamily::Serif, TextFamily::Mono] {
            painted(surface, Some(&mut faces), &frame(&[(family, pixels)]));
            made += 1;
            assert_eq!(faces.pages_made(), made);
            assert_eq!(faces.pages_held(), (made as usize).min(PAGES));
        }
    }
    // The scale's own size takes no page; a new scale starts afresh.
    painted(surface, Some(&mut faces), &frame(&[(TextFamily::Sans, 16)]));
    assert_eq!(faces.pages_made(), made);
    let double = Surface::new(400, 400, scale(2)).unwrap();
    painted(double, Some(&mut faces), &frame(&[(TextFamily::Sans, 16)]));
    assert_eq!((faces.pages_held(), faces.pages_made()), (1, 1));
}

#[test]
fn a_page_remade_after_eviction_draws_as_one_made_fresh() {
    let surface = Surface::new(400, 400, scale(1)).unwrap();
    let eldest = frame(&[(TextFamily::Serif, 30)]);
    let mut fresh = faces();
    let expected = painted(surface, Some(&mut fresh), &eldest);
    let mut faces = faces();
    assert_eq!(painted(surface, Some(&mut faces), &eldest), expected);
    // Eight more sizes: the ninth page lets the eldest, 30, go.
    for pixels in 31..=38 {
        painted(
            surface,
            Some(&mut faces),
            &frame(&[(TextFamily::Serif, pixels)]),
        );
    }
    assert_eq!((faces.pages_made(), faces.pages_held()), (9, PAGES));
    // 31 was drawn from longest ago now; 30 is made again, equal.
    assert_eq!(painted(surface, Some(&mut faces), &eldest), expected);
    assert_eq!(faces.pages_made(), 10);
    // 38, drawn last before it, was kept: no page made for it.
    painted(
        surface,
        Some(&mut faces),
        &frame(&[(TextFamily::Serif, 38)]),
    );
    assert_eq!(faces.pages_made(), 10);
}

#[test]
fn a_frame_needing_more_than_the_cap_draws_as_each_size_alone() {
    let surface = Surface::new(400, 400, scale(1)).unwrap();
    let mut combos = Vec::new();
    for pixels in [12, 20, 24, 28, 36, 40] {
        for family in [TextFamily::Sans, TextFamily::Serif] {
            combos.push((family, pixels));
        }
    }
    assert!(combos.len() > PAGES);
    let draws = frame(&combos);
    // Each draw with faces of its own, which never evict.
    let font = pinned().unwrap();
    let mut expected = vec![GARBAGE; 400 * 400 * 4];
    for draw in &draws {
        let mut own = faces();
        Raster::new(&mut expected, &font, surface, 1600)
            .unwrap()
            .with_prose(Some(&mut own))
            .draw(*draw);
    }
    let mut faces = faces();
    for round in 1..=3 {
        assert_eq!(
            painted(surface, Some(&mut faces), &draws),
            expected,
            "frame {round}"
        );
        // Drawn in a cycle past the cap, every page is remade each frame.
        assert_eq!(faces.pages_made(), 12 * round);
        assert_eq!(faces.pages_held(), PAGES);
    }
}

/// Prose lines at their tops, painted.
struct Lines {
    surface: Surface,
    lines: Vec<(Line, i64)>,
}

impl Composition for Lines {
    fn surface(&self) -> Surface {
        self.surface
    }
    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        for (line, y) in &self.lines {
            draw(line, (7, *y), self.surface.bounds(), damage, sink);
        }
    }
}

#[test]
fn measuring_and_drawing_agree_across_independently_made_faces() {
    let font = pinned().unwrap();
    let text = "mi mT ij 0─ é mm ii";
    for value in [1, 2] {
        let surface = Surface::new(600, 500, scale(value)).unwrap();
        // The measuring thread's faces and the drawing thread's, each
        // made from the bytes on its own.
        let mut measuring = faces_with_mono();
        let mut drawing = faces_with_mono();
        let mut metrics = Metrics::new(Some(&measuring), surface.scale);
        let mut lines = Vec::new();
        let mut top = 0;
        for (index, pixels) in [0, MIN_SIZE, 21, 45, 100].into_iter().enumerate() {
            let size = if pixels == 0 {
                Size::DEFAULT
            } else {
                Size::new(u32::from(pixels)).unwrap()
            };
            let family = [TextFamily::Sans, TextFamily::Serif, TextFamily::Mono][index % 3];
            let runs = [Run {
                text,
                style: sized(family, size).bold(),
                tag: 0,
            }];
            let laid = wrap(&mut metrics, &runs, 280 * u32::from(value));
            let mut buffer = vec![0; 600 * 500 * 4];
            let mut raster = Raster::new(&mut buffer, &font, surface, 2400)
                .unwrap()
                .with_prose(Some(&mut drawing));
            // The drawing side lays the same lines with its own metrics.
            assert_eq!(wrap(raster.metrics(), &runs, 280 * u32::from(value)), laid);
            for scalar in text.chars() {
                for style in [Style::Regular, Style::BoldItalic] {
                    assert_eq!(
                        raster.metrics().advance_at(size, family, style, scalar),
                        metrics.advance_at(size, family, style, scalar)
                    );
                }
            }
            assert_eq!(raster.metrics().line_at(size), metrics.line_at(size));
            let height = i64::from(metrics.line_at(size).height);
            for line in laid {
                lines.push((line, top));
                top += height;
            }
        }
        let page = Lines { surface, lines };
        let paint = |faces: &mut Faces| {
            let mut pixels = vec![GARBAGE; 600 * 500 * 4];
            Raster::new(&mut pixels, &font, surface, 2400)
                .unwrap()
                .with_prose(Some(faces))
                .paint(&page, surface.bounds())
                .unwrap();
            pixels
        };
        let drawn = paint(&mut drawing);
        assert!(drawn.chunks(4).any(|p| p != [GARBAGE; 4]));
        assert_eq!(drawn, paint(&mut measuring), "scale {value}");
    }
}

#[test]
fn the_page_drawn_last_is_kept_and_the_eldest_let_go() {
    let surface = Surface::new(400, 400, scale(1)).unwrap();
    let mut faces = faces();
    let draw_at = |faces: &mut Faces, pixels: u16| {
        painted(surface, Some(faces), &frame(&[(TextFamily::Serif, pixels)]));
    };
    for pixels in 30..=37 {
        draw_at(&mut faces, pixels);
    }
    assert_eq!((faces.pages_made(), faces.pages_held()), (8, PAGES));
    // 30 drawn again is the newest: the ninth size lets 31 go instead.
    draw_at(&mut faces, 30);
    draw_at(&mut faces, 38);
    assert_eq!(faces.pages_made(), 9);
    draw_at(&mut faces, 30);
    assert_eq!(faces.pages_made(), 9, "30 kept");
    draw_at(&mut faces, 31);
    assert_eq!(faces.pages_made(), 10, "31 let go");
    // 31 took 32's place, the eldest then; 30 is still held.
    draw_at(&mut faces, 30);
    assert_eq!(faces.pages_made(), 10);
    draw_at(&mut faces, 32);
    assert_eq!(faces.pages_made(), 11);
}

#[test]
fn a_page_of_another_size_resets_at_its_own_keys_made_and_remade() {
    let surface = Surface::new(400, 400, scale(1)).unwrap();
    let mut faces = faces();
    assert_eq!(faces.page_keys(), None);
    for pixels in 30..=40 {
        painted(
            surface,
            Some(&mut faces),
            &frame(&[(TextFamily::Sans, pixels)]),
        );
        assert_eq!(faces.page_keys(), Some(PAGE_KEYS), "{pixels}");
    }
    // Eleven sizes through eight places: three made again in another's.
    assert_eq!(faces.pages_made(), 11);
    const { assert!(PAGE_KEYS < td_ui::atlas::MAX_KEYS) };
}

#[test]
fn a_style_at_its_own_units_per_em_scales_by_them_at_every_size() {
    // Bold at 2000 units per em: its 'm' 0.6 em where Regular's is 0.8.
    let bold = encoded_per_em(
        &[
            ('0', Glyph::Simple(vec![rectangle(0, 0, 1000, 1000)]), 1200),
            ('m', Glyph::Simple(vec![rectangle(0, 0, 1000, 1000)]), 1200),
        ],
        1600,
        -600,
        2000,
    );
    let faces = Faces::new([sans(), bold, sans(), sans()], four(serif())).unwrap();
    let mut metrics = Metrics::new(Some(&faces), scale(1));
    for pixels in [MIN_SIZE, 16, 50, MAX_SIZE] {
        let size = Size::new(u32::from(pixels)).unwrap();
        assert_eq!(
            metrics.advance_at(size, TextFamily::Sans, Style::Bold, 'm'),
            at(600, i64::from(pixels)),
            "{pixels}"
        );
        assert_eq!(
            metrics.advance_at(size, TextFamily::Sans, Style::Regular, 'm'),
            at(800, i64::from(pixels)),
            "{pixels}"
        );
    }
    // A scalar Bold lacks is measured from Regular, at Regular's units.
    let size = Size::new(50).unwrap();
    assert_eq!(
        metrics.advance_at(size, TextFamily::Sans, Style::Bold, 'i'),
        metrics.advance_at(size, TextFamily::Sans, Style::Regular, 'i')
    );
}

#[test]
fn faces_refuse_a_family_that_does_not_size_at_either_end_of_the_bound() {
    // A line box 2.8 em high: 179 pixels at 64, 538 past a cell's 512 at
    // 192.
    let tall = encoded(
        &[('0', Glyph::Simple(vec![rectangle(0, 0, 500, 500)]), 600)],
        2000,
        -800,
    );
    assert_eq!(
        Faces::new(four(tall.clone()), four(serif())).unwrap_err(),
        Refused {
            family: TextFamily::Sans,
            style: Style::Regular,
            error: Error::Limit("cell"),
        }
    );
    // A '0' 50 units wide: no pixel at 6 px/em, one at 16.
    let narrow = encoded(
        &[('0', Glyph::Simple(vec![rectangle(0, 0, 40, 500)]), 50)],
        800,
        -200,
    );
    assert_eq!(
        Faces::new(four(sans()), four(narrow.clone())).unwrap_err(),
        Refused {
            family: TextFamily::Serif,
            style: Style::Regular,
            error: Error::Malformed("cell metrics"),
        }
    );
    // The monospace face is held to the same ends.
    for (bytes, error) in [
        (tall, Error::Limit("cell")),
        (narrow, Error::Malformed("cell metrics")),
    ] {
        let typeface = Typeface::new(bytes, None).unwrap();
        assert_eq!(faces().with_mono(&typeface).unwrap_err(), error);
    }
}

#[test]
fn a_glyph_past_the_mask_axis_at_a_size_draws_from_its_family_inside_its_advance() {
    // 'W' 3000 units wide: 576 pixels at 192, past a mask's 512.
    let wide = encoded(
        &[
            ('0', Glyph::Simple(vec![rectangle(0, 0, 500, 500)]), 600),
            ('W', Glyph::Simple(vec![rectangle(0, 0, 3000, 500)]), 3000),
        ],
        800,
        -300,
    );
    let mut faces = Faces::new(four(wide), four(serif()))
        .unwrap()
        .with_mono(&mono())
        .unwrap();
    let surface = Surface::new(700, 300, scale(1)).unwrap();
    let size = Size::new(u32::from(MAX_SIZE)).unwrap();
    let mut metrics = Metrics::new(Some(&faces), surface.scale);
    assert_eq!(
        metrics.source(TextFamily::Sans, Style::Regular, 'W'),
        Source::Family
    );
    assert_eq!(
        metrics.advance_at(size, TextFamily::Sans, Style::Regular, 'W'),
        576 * UNIT
    );
    let draws = [glyph(
        surface.bounds(),
        (20, 4),
        'W',
        sized(TextFamily::Sans, size),
    )];
    for _ in 0..3 {
        let drawn = painted(surface, Some(&mut faces), &draws);
        let ((left, right), _) = ink(&drawn, surface).unwrap();
        assert!(left >= 20 && right < 20 + 576, "{left}..{right}");
        assert!(right - left + 1 >= 500, "covered smaller, not Unifont");
    }
    // Its family's page alone, made once: no monospace page tried.
    assert_eq!((faces.pages_made(), faces.pages_held()), (1, 1));
}

#[test]
fn reach_bounds_the_ink_past_the_line_box_at_each_size() {
    // Sans says, and draws, a 'g' reaching 600 units below its baseline,
    // twice its descender.
    let deep = encoded_reaching(
        &[
            ('0', Glyph::Simple(vec![rectangle(0, 0, 500, 500)]), 600),
            ('g', Glyph::Simple(vec![rectangle(0, -600, 500, 500)]), 600),
        ],
        800,
        -300,
        (-600, 800),
    );
    let mut faces = Faces::new(four(deep), four(serif())).unwrap();
    let surface = Surface::new(200, 300, scale(1)).unwrap();
    let metrics = Metrics::new(Some(&faces), surface.scale);
    for pixels in [MIN_SIZE, 20, 100] {
        let size = Size::new(u32::from(pixels)).unwrap();
        let line = metrics.line_at(size);
        let reach = metrics.reach_at(size);
        let top = 120;
        for scalar in ['g', 'é'] {
            let drawn = painted(
                surface,
                Some(&mut faces),
                &[glyph(
                    surface.bounds(),
                    (10, top),
                    scalar,
                    sized(TextFamily::Sans, size),
                )],
            );
            let (_, (high, low)) = ink(&drawn, surface).unwrap();
            assert!(
                high as i64 >= top - i64::from(reach.above),
                "{scalar} {pixels}"
            );
            let bottom = top + i64::from(line.height) + i64::from(reach.below);
            assert!((low as i64) < bottom, "{scalar} {pixels}");
        }
    }
    // 'g' at 100: 60 pixels below the baseline, 30 past the line's 30.
    let reach = metrics.reach_at(Size::new(100).unwrap());
    assert_eq!(reach.below, 31);
    // At 6 the bitmap face's cell rises 14 rows over a 5-pixel ascent.
    assert_eq!(metrics.reach_at(Size::new(6).unwrap()).above, 9);
    // On the cell nothing passes the line.
    let cell = Metrics::new(None, scale(2));
    assert_eq!(cell.reach_at(Size::new(100).unwrap()), Reach::default());
}

#[test]
fn glyphs_of_several_sizes_share_a_baseline_at_their_tops() {
    let mut faces = faces();
    let surface = Surface::new(300, 200, scale(1)).unwrap();
    let metrics = Metrics::new(Some(&faces), surface.scale);
    let baseline = 150;
    let mut draws = Vec::new();
    for (column, pixels) in [12, 40, 100].into_iter().enumerate() {
        let size = Size::new(pixels).unwrap();
        let top = metrics.top_at(size, baseline);
        assert_eq!(top + i64::from(metrics.line_at(size).baseline), baseline);
        draws.push(glyph(
            surface.bounds(),
            (10 + column as i64 * 100, top),
            '0',
            sized(TextFamily::Sans, size),
        ));
    }
    // Each '0' stands on the baseline: its last inked row is the one
    // above it.
    let drawn = painted(surface, Some(&mut faces), &draws);
    for column in 0..3 {
        let x = 12 + column * 100;
        let lowest = (0..200)
            .filter(|&y| !untouched(&drawn, surface, (x, y)))
            .max()
            .unwrap();
        assert_eq!(lowest, baseline as usize - 1, "column {column}");
    }
}

#[test]
fn the_atlas_bound_counts_pages_key_maps_shelves_and_scratch() {
    // A key map node: the ordered map's eleven keys and slots and twelve
    // edges, which the bound takes as 768 bytes.
    let pair = std::mem::size_of::<(Style, char)>() + std::mem::size_of::<td_ui::atlas::Slot>();
    assert!(16 + 11 * pair + 12 * 8 <= 768, "{pair}");
    let page = PAGE_WIDTH * PAGE_HEIGHT + (PAGE_KEYS / 4 + 2) * 768 + PAGE_HEIGHT * 48;
    assert_eq!(ATLAS, PAGES * page + (3 << 20));
    // td-web's jail term and td-ui's design state it.
    assert_eq!(ATLAS, 13_512_704);
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
