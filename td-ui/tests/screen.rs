#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! Draw-stream and pixel oracles for the cell screen: the grid it lays
//! out over a surface, the ground, run fills and glyphs it streams inside
//! a damage rectangle, the pixels a raster paints from it, the text the
//! driven seam reads back, and the key vocabulary its chords translate to.

use td_ui::driven;
use td_ui::font;
use td_ui::raster::{Composition, Draw, Primitive, Raster, Rect, Scale, Surface, Weight};
use td_ui::screen::{press, Cell, Input, Key, Press, Screen, Style, INK, PAPER};

const BLUE: u32 = 0x0033aa;
const RED: u32 = 0xaa3300;

fn surface(width: usize, height: usize, scale: u8) -> Surface {
    Surface::new(width, height, Scale::new(scale).unwrap()).unwrap()
}

fn draws(screen: &Screen, damage: Rect) -> Vec<Draw> {
    let mut out = Vec::new();
    screen.emit(damage, &mut |draw| out.push(draw));
    out
}

fn fills(draws: &[Draw]) -> Vec<(Rect, u32)> {
    draws
        .iter()
        .filter_map(|d| match d.primitive {
            Primitive::Fill { rect, color } => Some((rect, color)),
            Primitive::Glyph { .. } => None,
        })
        .collect()
}

fn glyphs(draws: &[Draw]) -> Vec<(i64, i64, char, u32, u32, Weight)> {
    draws
        .iter()
        .filter_map(|d| match d.primitive {
            Primitive::Glyph {
                x,
                y,
                scalar,
                style,
            } => Some((x, y, scalar, style.ink, style.background, style.weight)),
            Primitive::Fill { .. } => None,
        })
        .collect()
}

fn paint(screen: &Screen) -> Vec<u8> {
    let surface = screen.surface();
    let mut pixels = vec![0u8; surface.width * surface.height * 4];
    let font = font::pinned().unwrap();
    let mut raster = Raster::new(&mut pixels, &font, surface, surface.width * 4).unwrap();
    raster.paint(screen, surface.bounds()).unwrap();
    pixels
}

fn pixel(pixels: &[u8], width: usize, x: usize, y: usize) -> u32 {
    let at = (y * width + x) * 4;
    u32::from_le_bytes(pixels[at..at + 4].try_into().unwrap()) & 0xff_ffff
}

#[test]
fn the_grid_is_laid_out_in_cells_over_the_surface_and_the_remainder_is_ground() {
    let mut screen = Screen::new(surface(83, 35, 1), Style::default()).unwrap();
    assert_eq!((screen.rows(), screen.columns()), (2, 10));
    assert_eq!(screen.ground(), PAPER);
    assert_eq!(
        screen.cell(1, 9),
        Some(Cell {
            scalar: ' ',
            style: Style::default()
        })
    );
    assert_eq!(screen.cell(2, 0), None);
    assert_eq!(screen.cell(0, 10), None);
    // Writes stop at the right edge; a control scalar is the replacement.
    assert_eq!(screen.write(1, 8, "abc", Style::default()), 2);
    assert!(screen.put(0, 0, '\u{7}', Style::default()));
    assert_eq!(screen.cell(0, 0).unwrap().scalar, '\u{fffd}');
    assert_eq!(screen.line(1).as_deref(), Some("        ab"));
    assert_eq!(screen.line(2), None);
    // A row is cleared from a column in a style; the ground follows clear
    // alone, so a status row leaves the surface around the grid as it was.
    screen.clear_row(1, 9, Style::new(INK, BLUE));
    assert_eq!(screen.ground(), PAPER);
    assert_eq!(screen.cell(1, 8).unwrap().scalar, 'a');
    assert_eq!(
        screen.cell(1, 9).unwrap(),
        Cell {
            scalar: ' ',
            style: Style::new(INK, BLUE)
        }
    );
    screen.clear(Style::new(PAPER, INK));
    assert_eq!(screen.ground(), INK);
    assert_eq!(
        screen.cell(1, 8).unwrap(),
        Cell {
            scalar: ' ',
            style: Style::new(PAPER, INK)
        }
    );
    // Hits map pixels to cells inside the grid only.
    assert_eq!(screen.hit(0, 0), Some((0, 0)));
    assert_eq!(screen.hit(79, 31), Some((1, 9)));
    assert_eq!(screen.hit(80, 0), None);
    assert_eq!(screen.hit(0, 32), None);
    assert_eq!(screen.hit(-1, 5), None);
    // A resize relays out and clears; a surface under a cell is no cells.
    screen.resize(surface(16, 48, 2), Style::default()).unwrap();
    assert_eq!((screen.rows(), screen.columns()), (1, 1));
    assert_eq!(screen.hit(15, 31), Some((0, 0)));
    assert_eq!(screen.hit(15, 32), None);
    screen.resize(surface(7, 15, 1), Style::default()).unwrap();
    assert_eq!((screen.rows(), screen.columns()), (0, 0));
    assert!(draws(&screen, screen.surface().bounds()).len() == 1);
    assert!(Screen::new(
        Surface {
            width: 0,
            height: 16,
            scale: Scale::default()
        },
        Style::default()
    )
    .is_err());
    // A surface under a cell on one axis alone is no cells on both.
    for (width, height) in [(7, 480), (800, 15)] {
        screen
            .resize(surface(width, height, 1), Style::default())
            .unwrap();
        assert_eq!(
            (screen.rows(), screen.columns()),
            (0, 0),
            "{width}x{height}"
        );
        assert_eq!(screen.line(0), None);
        assert_eq!(screen.hit(0, 0), None);
    }
}

#[test]
fn a_row_keeps_a_space_like_scalar_a_program_wrote_and_loses_only_blank_cells() {
    let mut screen = Screen::new(surface(80, 16, 1), Style::default()).unwrap();
    screen.write(0, 0, "a\u{a0}", Style::default());
    assert_eq!(screen.line(0).as_deref(), Some("a\u{a0}"));
    screen.write(0, 4, "b ", Style::default());
    assert_eq!(screen.line(0).as_deref(), Some("a\u{a0}  b"));
    // The driven seam's read-back trims whitespace of any kind, so the
    // one scalar `line` keeps at a row's end is the one it drops.
    screen.clear(Style::default());
    screen.write(0, 0, "a\u{a0}", Style::default());
    let (_, _, text) = driven::text(&screen).unwrap();
    assert_eq!(text, "a");
}

#[test]
fn the_stream_culls_the_columns_outside_the_damage_as_it_culls_the_rows() {
    let mut screen = Screen::new(surface(80, 32, 1), Style::default()).unwrap();
    let selected = Style::new(PAPER, BLUE);
    screen.write(0, 0, "abcdefghij", selected);
    screen.write(1, 0, "0123456789", Style::default());
    // Damage over columns three and four of both rows: the ground, one
    // fill cut to the damaged span of the first row's run, and four glyphs.
    let damage = Rect {
        x: 24,
        y: 0,
        width: 16,
        height: 32,
    };
    let stream = draws(&screen, damage);
    assert_eq!(
        fills(&stream),
        [
            (screen.surface().bounds(), PAPER),
            (
                Rect {
                    x: 24,
                    y: 0,
                    width: 16,
                    height: 16
                },
                BLUE
            )
        ]
    );
    assert_eq!(
        glyphs(&stream)
            .iter()
            .map(|glyph| (glyph.0, glyph.1, glyph.2))
            .collect::<Vec<_>>(),
        [(24, 0, 'd'), (32, 0, 'e'), (24, 16, '3'), (32, 16, '4')]
    );
    // A damage edge inside a cell still takes the whole cell.
    let stream = draws(
        &screen,
        Rect {
            x: 27,
            y: 20,
            width: 1,
            height: 1,
        },
    );
    assert_eq!(glyphs(&stream).len(), 1);
    assert_eq!(glyphs(&stream)[0].2, '3');
    // The pixels are the same as an unculled paint of the damage.
    let font = font::pinned().unwrap();
    let full = paint(&screen);
    let mut pixels = vec![0u8; 80 * 32 * 4];
    let mut raster = Raster::new(&mut pixels, &font, screen.surface(), 80 * 4).unwrap();
    raster.paint(&screen, damage).unwrap();
    for y in 0..32 {
        for x in 24..40 {
            assert_eq!(pixel(&pixels, 80, x, y), pixel(&full, 80, x, y), "{x},{y}");
        }
    }
}

#[test]
fn the_stream_is_the_ground_then_run_fills_and_glyphs_inside_the_damage() {
    let mut screen = Screen::new(surface(80, 32, 1), Style::default()).unwrap();
    let selected = Style::new(PAPER, BLUE);
    screen.write(0, 1, "ab", selected);
    screen.put(0, 3, ' ', selected);
    screen.write(1, 0, "x y", Style::default().bold());
    screen.put(1, 4, 'z', Style::new(RED, PAPER));
    let all = draws(&screen, screen.surface().bounds());
    let bounds = screen.surface().bounds();
    assert!(all.iter().all(|d| d.clip == bounds));
    // The ground first, then one fill for the run of three selected cells.
    assert_eq!(
        fills(&all),
        [
            (bounds, PAPER),
            (
                Rect {
                    x: 8,
                    y: 0,
                    width: 24,
                    height: 16
                },
                BLUE
            )
        ]
    );
    assert_eq!(
        glyphs(&all),
        [
            (8, 0, 'a', PAPER, BLUE, Weight::Regular),
            (16, 0, 'b', PAPER, BLUE, Weight::Regular),
            (0, 16, 'x', INK, PAPER, Weight::Medium),
            (16, 16, 'y', INK, PAPER, Weight::Medium),
            (32, 16, 'z', RED, PAPER, Weight::Regular),
        ]
    );
    // Damage over the second row only: the ground clipped to it, no fill
    // and no glyph from the first row.
    let damage = Rect {
        x: 0,
        y: 16,
        width: 80,
        height: 16,
    };
    let lower = draws(&screen, damage);
    assert!(lower.iter().all(|d| d.clip == damage));
    assert_eq!(fills(&lower), [(bounds, PAPER)]);
    assert_eq!(glyphs(&lower).len(), 3);
    assert!(draws(
        &screen,
        Rect {
            x: 100,
            y: 0,
            width: 8,
            height: 8
        }
    )
    .is_empty());
    // At scale two the cells are twice the size.
    let mut big = Screen::new(surface(64, 64, 2), Style::default()).unwrap();
    big.put(1, 2, 'q', Style::new(INK, BLUE));
    let stream = draws(&big, big.surface().bounds());
    assert_eq!(
        fills(&stream)[1],
        (
            Rect {
                x: 32,
                y: 32,
                width: 16,
                height: 32
            },
            BLUE
        )
    );
    assert_eq!(glyphs(&stream), [(32, 32, 'q', INK, BLUE, Weight::Regular)]);
}

#[test]
fn a_screen_paints_its_cells_to_pixels_and_the_remainder_in_the_ground() {
    let mut screen = Screen::new(surface(83, 35, 1), Style::new(INK, PAPER)).unwrap();
    screen.put(1, 2, 'H', Style::new(PAPER, BLUE));
    screen.put(0, 9, ' ', Style::new(INK, RED));
    let pixels = paint(&screen);
    let at = |x, y| pixel(&pixels, 83, x, y);
    // The remainder beyond the grid and a blank cell are the ground.
    assert_eq!(at(82, 34), PAPER);
    assert_eq!(at(0, 33), PAPER);
    assert_eq!(at(4, 8), PAPER);
    // A blank cell in another background is that background whole.
    for x in 72..80 {
        for y in 0..16 {
            assert_eq!(at(x, y), RED, "{x},{y}");
        }
    }
    // The glyph cell: its background everywhere the face is unlit, its
    // ink where lit, and nothing outside it.
    let font = font::pinned().unwrap();
    let index = font.index('H');
    let mut lit = 0;
    for column in 0..8 {
        for row in 0..16 {
            let expected = if font.pixel(index, column, row) {
                lit += 1;
                PAPER
            } else {
                BLUE
            };
            assert_eq!(at(16 + column, 16 + row), expected, "{column},{row}");
        }
    }
    assert!(lit > 0);
    assert_eq!(at(15, 20), PAPER);
    assert_eq!(at(24, 20), PAPER);
}

#[test]
fn the_driven_seam_reads_the_screen_back_as_its_lines() {
    let mut screen = Screen::new(surface(80, 48, 1), Style::default()).unwrap();
    screen.write(0, 0, "Feeds", Style::default().bold());
    screen.write(2, 3, "[All]", Style::default().reversed());
    let (rows, columns, text) = driven::text(&screen).unwrap();
    assert_eq!((rows, columns), (3, 10));
    assert_eq!(text, "Feeds\n\n   [All]");
    assert_eq!(screen.line(2).as_deref(), Some("   [All]"));
}

#[test]
fn styles_fold_and_reverse_and_presses_translate_the_keyboards_chords() {
    let style = Style::new(INK, PAPER);
    assert_eq!(style.bold().weight, Weight::Medium);
    assert_eq!(style.reversed(), Style::new(PAPER, INK));
    assert_eq!(style.bold().reversed().weight, Weight::Medium);
    assert_eq!(Style::default(), style);
    let plain = |key| Some(Press::plain(key));
    assert_eq!(press("a"), plain(Key::Char('a')));
    assert_eq!(press("Z"), plain(Key::Char('Z')));
    assert_eq!(press("?"), plain(Key::Char('?')));
    assert_eq!(press("Space"), plain(Key::Char(' ')));
    assert_eq!(press("Return"), plain(Key::Enter));
    assert_eq!(press("Escape"), plain(Key::Escape));
    assert_eq!(press("Backspace"), plain(Key::Backspace));
    assert_eq!(press("Tab"), plain(Key::Tab));
    assert_eq!(press("PageDown"), plain(Key::PageDown));
    assert_eq!(press("Insert"), plain(Key::Insert));
    assert_eq!(press("Delete"), plain(Key::Delete));
    assert_eq!(press("F7"), plain(Key::Function(7)));
    assert_eq!(
        press("C-c"),
        Some(Press {
            key: Key::Char('c'),
            control: true,
            alt: false,
            shift: false
        })
    );
    assert_eq!(
        press("M-Return"),
        Some(Press {
            key: Key::Enter,
            control: false,
            alt: true,
            shift: false
        })
    );
    assert_eq!(
        press("C-S-Left"),
        Some(Press {
            key: Key::Left,
            control: true,
            alt: false,
            shift: true
        })
    );
    assert_eq!(press("C-a").unwrap().plain_char(), None);
    assert_eq!(press("S-a").unwrap().plain_char(), None);
    // A prefix's own terminator as the key, and a doubled prefix with none.
    assert_eq!(
        press("C--"),
        Some(Press {
            key: Key::Char('-'),
            control: true,
            alt: false,
            shift: false
        })
    );
    assert_eq!(press("C-M-"), None);
    // The keyboard spells an unmodified space bar as the space itself and
    // names it only under a modifier; both are the one key.
    assert_eq!(press(" "), plain(Key::Char(' ')));
    assert_eq!(press(" ").unwrap().plain_char(), Some(' '));
    assert_eq!(press("C-Space").unwrap().key, Key::Char(' '));
    // A literal space under a modifier is a spelling the keyboard never
    // emits, so the grammar stays canonical: `C-Space` is the chord.
    for refused in ["F+5", "F09", "F1a", "F 1", "  ", "C- ", "M- ", "S- ", "C-M- "] {
        assert_eq!(press(refused), None, "{refused:?}");
    }
    assert_eq!(press("a").unwrap().plain_char(), Some('a'));
    assert_eq!(press("Up").unwrap().plain_char(), None);
    for refused in [
        "", "F0", "F13", "Fx", "Meta", "C-", "S-", "é", "ab", "Space ",
    ] {
        assert_eq!(press(refused), None, "{refused:?}");
    }
    assert_eq!(press(&"a".repeat(driven::KEY_BYTES + 1)), None);
    // The input vocabulary is plain data.
    let input = Input::Wheel {
        rows: -3,
        columns: 0,
    };
    assert_eq!(input, input);
    assert_ne!(Input::Close, Input::Focus(false));
}
