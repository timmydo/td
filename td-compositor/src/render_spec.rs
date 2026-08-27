use super::*;
use crate::keys;
use std::path::PathBuf;
use std::sync::OnceLock;

/// One decode of the pinned face for the whole spec; `Font` is plain data.
fn face() -> &'static Font {
    static FONT: OnceLock<Font> = OnceLock::new();
    FONT.get_or_init(|| crate::font::pinned().unwrap())
}

fn palette() -> &'static Palette {
    static PALETTE: OnceLock<Palette> = OnceLock::new();
    PALETTE.get_or_init(Palette::pinned)
}

/// The three numbers a viewport reads, as the model reports them.
fn scrollback(terminal: &Terminal) -> keys::Scrollback {
    keys::Scrollback {
        epoch: terminal.history_epoch(),
        pushed: terminal.history_pushed(),
        lines: terminal.history_lines(),
    }
}

fn terminal(rows: usize, columns: usize, input: &[u8]) -> Terminal {
    let mut terminal = Terminal::new(rows, columns).unwrap();
    terminal.feed(input);
    terminal
}

fn surface(rows: usize, columns: usize) -> (usize, usize) {
    (columns * face().width(), rows * face().height())
}

fn draw(snapshot: &Snapshot, width: usize, height: usize) -> Vec<u8> {
    let mut pixels = vec![0; width * height * BYTES_PER_PIXEL];
    render(snapshot, palette(), face(), &mut pixels, width, height).unwrap();
    pixels
}

/// Render one grid at exactly its natural size.
fn draw_grid(snapshot: &Snapshot) -> (Vec<u8>, usize, usize) {
    let (width, height) = surface(snapshot.rows(), snapshot.columns());
    (draw(snapshot, width, height), width, height)
}

fn rgb_at(pixels: &[u8], width: usize, x: usize, y: usize) -> [u8; 3] {
    let offset = (y * width + x) * BYTES_PER_PIXEL;
    let pixel = pixels.get(offset..offset + BYTES_PER_PIXEL).unwrap();
    [
        *pixel.get(2).unwrap(),
        *pixel.get(1).unwrap(),
        *pixel.first().unwrap(),
    ]
}

fn set_pixels(pixels: &[u8], width: usize, ink: [u8; 3]) -> Vec<(usize, usize)> {
    let height = pixels.len() / BYTES_PER_PIXEL / width;
    let mut found = Vec::new();
    for y in 0..height {
        for x in 0..width {
            if rgb_at(pixels, width, x, y) == ink {
                found.push((x, y));
            }
        }
    }
    found
}

// ---------------------------------------------------------------- palette

#[test]
fn the_palette_pins_xterms_sixteen_base_entries() {
    let palette = palette();
    assert_eq!(palette.entry(0), [0x00, 0x00, 0x00]);
    assert_eq!(palette.entry(1), [0xcd, 0x00, 0x00]);
    assert_eq!(palette.entry(4), [0x00, 0x00, 0xee]);
    assert_eq!(palette.entry(7), [0xe5, 0xe5, 0xe5]);
    assert_eq!(palette.entry(8), [0x7f, 0x7f, 0x7f]);
    assert_eq!(palette.entry(12), [0x5c, 0x5c, 0xff]);
    assert_eq!(palette.entry(15), [0xff, 0xff, 0xff]);
}

#[test]
fn the_palette_cube_is_the_six_level_product_of_its_axes() {
    let palette = palette();
    assert_eq!(palette.entry(16), [0, 0, 0]);
    assert_eq!(palette.entry(231), [255, 255, 255]);
    // 16 + 36*1 + 6*2 + 3
    assert_eq!(palette.entry(67), [95, 135, 175]);
    for red in 0..6usize {
        for green in 0..6usize {
            for blue in 0..6usize {
                let index = 16 + 36 * red + 6 * green + blue;
                let entry = palette.entry(u8::try_from(index).unwrap());
                assert_eq!(
                    entry,
                    [
                        CUBE_LEVELS[red % 6],
                        CUBE_LEVELS[green % 6],
                        CUBE_LEVELS[blue % 6]
                    ],
                    "cube entry {index}"
                );
            }
        }
    }
}

#[test]
fn the_palette_regions_tile_all_two_hundred_fifty_six_entries() {
    // `cube_level`'s modulo and the ramp's saturation both turn a moved
    // boundary into a wrong colour rather than a failure, so the three
    // regions are pinned to abut exactly and to cover the space.
    assert_eq!(BASE.len(), CUBE_START);
    assert_eq!(CUBE_START + CUBE_LEVELS.len().pow(3), RAMP_START);
    assert_eq!(RAMP_START + 24, 256);
}

#[test]
fn the_palette_grey_ramp_runs_from_eight_to_two_hundred_thirty_eight() {
    let palette = palette();
    assert_eq!(palette.entry(232), [8, 8, 8]);
    assert_eq!(palette.entry(255), [238, 238, 238]);
    for step in 0..24u8 {
        let grey = 8 + step * 10;
        assert_eq!(palette.entry(232 + step), [grey, grey, grey]);
    }
}

#[test]
fn default_ink_is_palette_entry_seven_on_entry_zero() {
    let palette = palette();
    assert_eq!(palette.foreground(), palette.entry(7));
    assert_eq!(palette.background(), palette.entry(0));
}

#[test]
fn the_palette_resolves_every_color_form() {
    let palette = palette();
    assert_eq!(palette.resolve(Color::Default, [1, 2, 3]), [1, 2, 3]);
    assert_eq!(palette.resolve(Color::Indexed(9), [1, 2, 3]), [0xff, 0, 0]);
    assert_eq!(palette.resolve(Color::Rgb(4, 5, 6), [1, 2, 3]), [4, 5, 6]);
}

// ------------------------------------------------------------------- ink

fn attributes() -> Attributes {
    BLANK.attributes
}

#[test]
fn inverse_exchanges_foreground_and_background() {
    let mut plain = attributes();
    plain.foreground = Color::Indexed(1);
    plain.background = Color::Indexed(4);
    let mut inverse = plain;
    inverse.inverse = true;

    let plain = Ink::new(&plain, palette());
    let inverse = Ink::new(&inverse, palette());
    assert_eq!(inverse.foreground, plain.background);
    assert_eq!(inverse.background, plain.foreground);
}

#[test]
fn faint_blends_exactly_halfway_toward_the_background() {
    let mut faint = attributes();
    faint.foreground = Color::Rgb(200, 100, 51);
    faint.background = Color::Rgb(0, 0, 0);
    faint.faint = true;
    let ink = Ink::new(&faint, palette());
    assert_eq!(ink.foreground, [100, 50, 25]);
    assert_eq!(ink.background, [0, 0, 0]);
}

#[test]
fn faint_follows_the_inverse_exchange_rather_than_preceding_it() {
    let mut both = attributes();
    both.foreground = Color::Rgb(255, 255, 255);
    both.background = Color::Rgb(0, 0, 0);
    both.faint = true;
    both.inverse = true;
    let ink = Ink::new(&both, palette());
    // After the exchange the drawn foreground is black, and blending black
    // toward white would brighten it. Faint must dim.
    assert_eq!(ink.background, [255, 255, 255]);
    assert_eq!(ink.foreground, [127, 127, 127]);
}

#[test]
fn blend_half_matches_the_widened_average_for_every_channel_pair() {
    // Deliberately the OTHER formula: `blend_half` splits the halves and
    // carries the low bits to stay in `u8`, so widening here is an
    // independent oracle rather than a restatement of the code.
    for from in 0..=255u8 {
        for to in 0..=255u8 {
            let expected = u8::try_from((u16::from(from) + u16::from(to)) / 2).unwrap();
            assert_eq!(
                blend_half([from, from, from], [to, to, to]),
                [expected; 3],
                "{from}+{to}"
            );
        }
    }
}

#[test]
fn the_shear_leans_only_the_top_half_and_never_past_one_pixel() {
    for height in 1..64usize {
        for row in 0..height {
            let lean = shear(row, height);
            assert!(lean <= 1);
            assert_eq!(lean, usize::from(row < height / 2), "{row} of {height}");
        }
    }
}

// -------------------------------------------------------------- surfaces

#[test]
fn render_rejects_a_surface_whose_length_is_not_its_area() {
    let terminal = terminal(1, 1, b"A");
    let snapshot = Snapshot::new(&terminal, true, false);
    let mut pixels = vec![0; 3];
    let error = render(&snapshot, palette(), face(), &mut pixels, 8, 16).unwrap_err();
    assert!(error.contains("needs 512 bytes, not 3"), "{error}");
}

#[test]
fn render_reports_a_surface_whose_area_overflows() {
    let terminal = terminal(1, 1, b"A");
    let snapshot = Snapshot::new(&terminal, true, false);
    let mut pixels = Vec::new();
    let error = render(
        &snapshot,
        palette(),
        face(),
        &mut pixels,
        usize::MAX,
        usize::MAX,
    )
    .unwrap_err();
    assert!(error.contains("overflows a byte count"), "{error}");
}

#[test]
fn render_leaves_the_unused_byte_of_every_pixel_zero() {
    let terminal = terminal(2, 4, b"\x1b[41mtext");
    let snapshot = Snapshot::new(&terminal, true, true);
    let (pixels, ..) = draw_grid(&snapshot);
    let (chunks, rest) = pixels.as_chunks::<BYTES_PER_PIXEL>();
    assert!(rest.is_empty());
    assert!(chunks.iter().all(|pixel| pixel[3] == 0));
}

#[test]
fn render_paints_a_default_background_outside_the_grid() {
    let terminal = terminal(1, 1, b"A");
    let snapshot = Snapshot::new(&terminal, true, false);
    // Two cells of surface for a one-cell grid; the spare cell is background.
    let (width, height) = (face().width() * 2, face().height());
    let pixels = draw(&snapshot, width, height);
    for y in 0..height {
        for x in face().width()..width {
            assert_eq!(
                rgb_at(&pixels, width, x, y),
                palette().background(),
                "({x},{y})"
            );
        }
    }
}

#[test]
fn render_clips_a_grid_larger_than_its_surface() {
    let grid = terminal(4, 8, b"AAAAAAAA\r\nBBBBBBBB\r\nCCCCCCCC\r\nDDDDDDDD");
    let snapshot = hidden(&grid, false, false);
    // One cell of surface for a 4x8 grid: clipping, not an error.
    let (cell_width, cell_height) = surface(1, 1);
    let clipped = draw(&snapshot, cell_width, cell_height);
    let (full, full_width, _) = draw_grid(&snapshot);
    for y in 0..cell_height {
        for x in 0..cell_width {
            assert_eq!(
                rgb_at(&clipped, cell_width, x, y),
                rgb_at(&full, full_width, x, y),
                "({x},{y})"
            );
        }
    }
}

#[test]
fn render_paints_a_partially_visible_last_row_and_column() {
    let terminal = terminal(2, 2, b"AB\x1b[2;1HCD");
    let snapshot = Snapshot::new(&terminal, false, false);
    // One pixel short of two full cells on each axis: the last row and
    // column are half-drawn rather than dropped.
    let (full_width, full_height) = surface(2, 2);
    let (width, height) = (full_width - 1, full_height - 1);
    let pixels = draw(&snapshot, width, height);
    // Every pixel that survives is the one the full-size render put there,
    // which is stronger than "something was drawn past the first cell".
    let full = draw(&snapshot, full_width, full_height);
    for y in 0..height {
        for x in 0..width {
            assert_eq!(
                rgb_at(&pixels, width, x, y),
                rgb_at(&full, full_width, x, y),
                "({x},{y})"
            );
        }
    }
    let lit = set_pixels(&pixels, width, palette().foreground());
    assert!(
        lit.iter().any(|(x, _)| *x >= face().width()),
        "the clipped last column drew nothing"
    );
    assert!(
        lit.iter().any(|(_, y)| *y >= face().height()),
        "the clipped last row drew nothing"
    );
}

#[test]
fn a_face_with_no_area_does_not_parse() {
    let empty = Font::parse(&[
        0x72, 0xb5, 0x4a, 0x86, 0, 0, 0, 0, 32, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0,
    ]);
    // A zero-height face cannot parse at all, so the guard is unreachable
    // through the pinned reader; it is here because `render` takes any Font.
    assert!(empty.is_err());
}

// ------------------------------------------------------- rendition matrix

const RENDITIONS: &[(&str, &[u8])] = &[
    ("bold", b"\x1b[1m"),
    ("faint", b"\x1b[2m"),
    ("italic", b"\x1b[3m"),
    ("underline", b"\x1b[4m"),
    ("inverse", b"\x1b[7m"),
    ("strike", b"\x1b[9m"),
];

fn cell_of(select: &[u8], scalar: char) -> Vec<u8> {
    let mut input = select.to_vec();
    let mut buffer = [0u8; 4];
    input.extend_from_slice(scalar.encode_utf8(&mut buffer).as_bytes());
    let terminal = terminal(1, 1, &input);
    let snapshot = Snapshot::new(&terminal, false, false).with_cursor(Cursor {
        row: 0,
        column: 0,
        visible: false,
    });
    draw_grid(&snapshot).0
}

#[test]
fn every_claimed_rendition_differs_from_an_otherwise_identical_normal_cell() {
    let normal = cell_of(b"", 'A');
    for (name, select) in RENDITIONS {
        assert_ne!(
            cell_of(select, 'A'),
            normal,
            "{name} renders identically to normal"
        );
    }
}

#[test]
fn the_rendition_matrix_covers_exactly_the_models_attribute_flags() {
    // The destructure is the assertion: a seventh flag on `Attributes` is a
    // compile error here until it is named, so it cannot reach the renderer
    // without a presentation and a blocking case. Comparing the roster to a
    // second hardcoded list would pin nothing the list does not already say.
    let Attributes {
        bold,
        faint,
        italic,
        underline,
        inverse,
        strike,
        foreground: _,
        background: _,
    } = attributes();
    let flags = [
        ("bold", bold),
        ("faint", faint),
        ("italic", italic),
        ("underline", underline),
        ("inverse", inverse),
        ("strike", strike),
    ];
    let mut named: Vec<&str> = RENDITIONS.iter().map(|(name, _)| *name).collect();
    let mut declared: Vec<&str> = flags.iter().map(|(name, _)| *name).collect();
    named.sort_unstable();
    declared.sort_unstable();
    assert_eq!(named, declared);
    assert!(flags.iter().all(|(_, set)| !*set), "BLANK claims a rendition");
}

#[test]
fn bold_adds_a_one_pixel_rightward_copy_and_removes_nothing() {
    let normal = cell_of(b"", 'A');
    let bold = cell_of(b"\x1b[1m", 'A');
    let width = face().width();
    let ink = palette().foreground();
    let normal_lit = set_pixels(&normal, width, ink);
    let bold_lit = set_pixels(&bold, width, ink);
    assert!(!normal_lit.is_empty());
    for point in &normal_lit {
        assert!(bold_lit.contains(point), "bold dropped {point:?}");
    }
    assert!(bold_lit.len() > normal_lit.len());
    // Every added pixel is one step right of a set one.
    for (x, y) in &bold_lit {
        if normal_lit.contains(&(*x, *y)) {
            continue;
        }
        assert!(
            x.checked_sub(1)
                .is_some_and(|left| normal_lit.contains(&(left, *y))),
            "bold lit ({x},{y}) with nothing to its left"
        );
    }
}

#[test]
fn bold_clips_its_copy_at_the_cells_right_edge() {
    // U+2588 FULL BLOCK sets every column, so bold's copy would land in the
    // next cell if it were not clipped.
    // The spill has to be observable to be tested: within a grid the next
    // cell repaints its own background over anything that bled into it. So
    // this is a ONE-column grid on a two-cell surface, where the column to
    // the right of the glyph belongs to nobody and stays background.
    let heavy = terminal(1, 1, "\x1b[1m\u{2588}".as_bytes());
    let (width, height) = surface(1, 2);
    let pixels = draw(&hidden(&heavy, false, false), width, height);
    for y in 0..height {
        assert_eq!(
            rgb_at(&pixels, width, face().width() - 1, y),
            palette().foreground(),
            "the block's own last column at row {y}"
        );
        for x in face().width()..width {
            assert_eq!(
                rgb_at(&pixels, width, x, y),
                palette().background(),
                "bold bled to ({x},{y})"
            );
        }
    }
}

#[test]
fn italic_shears_the_top_half_one_pixel_right_and_leaves_the_bottom() {
    let normal = cell_of(b"", 'W');
    let italic = cell_of(b"\x1b[3m", 'W');
    let (width, height) = (face().width(), face().height());
    let background = palette().background();
    for y in 0..height {
        for x in 0..width {
            let sheared = rgb_at(&italic, width, x, y);
            if y >= height / 2 {
                assert_eq!(sheared, rgb_at(&normal, width, x, y), "({x},{y}) below");
                continue;
            }
            let expected = match x.checked_sub(1) {
                Some(source) => rgb_at(&normal, width, source, y),
                None => background,
            };
            assert_eq!(sheared, expected, "({x},{y}) above");
        }
    }
}

#[test]
fn underline_and_strike_rule_fixed_full_width_cell_rows() {
    let height = face().height();
    let width = face().width();
    let ink = palette().foreground();
    let background = palette().background();
    for (select, ruled) in [(b"\x1b[4m".as_slice(), height - 2), (b"\x1b[9m", height / 2)] {
        // A space has no glyph bits, so the rule is the only thing drawn.
        let cell = cell_of(select, ' ');
        for y in 0..height {
            for x in 0..width {
                let expected = if y == ruled { ink } else { background };
                assert_eq!(rgb_at(&cell, width, x, y), expected, "({x},{y}) row {ruled}");
            }
        }
    }
}

#[test]
fn underline_sits_two_rows_above_the_cells_bottom_edge() {
    let height = face().height();
    let cell = cell_of(b"\x1b[4m", ' ');
    let lit = set_pixels(&cell, face().width(), palette().foreground());
    assert!(lit.iter().all(|(_, y)| *y == height - 2));
}

#[test]
fn a_rendition_paints_the_cells_background_over_its_whole_area() {
    // `Cell::blank` keeps colours through an erase, so a coloured
    // background must survive to the pixels even with no glyph bits.
    let cell = cell_of(b"\x1b[44m", ' ');
    let width = face().width();
    for y in 0..face().height() {
        for x in 0..width {
            assert_eq!(rgb_at(&cell, width, x, y), palette().entry(4), "({x},{y})");
        }
    }
}

#[test]
fn twenty_four_bit_and_indexed_colors_reach_the_pixels() {
    let cell = cell_of(b"\x1b[48;2;17;34;51m", ' ');
    let width = face().width();
    assert_eq!(rgb_at(&cell, width, 0, 0), [17, 34, 51]);
    let cell = cell_of(b"\x1b[48;5;208m", ' ');
    assert_eq!(rgb_at(&cell, width, 0, 0), palette().entry(208));
}

#[test]
fn a_glyph_the_face_lacks_renders_a_visible_replacement() {
    // DESIGN section 10: a missing glyph renders a visible replacement cell
    // rather than a blank one.
    let missing = char::from_u32(0x10_fffd).unwrap();
    assert!(!face().covers(missing));
    let cell = cell_of(b"", missing);
    let lit = set_pixels(&cell, face().width(), palette().foreground());
    assert!(!lit.is_empty(), "a missing glyph drew nothing");
    assert_ne!(cell, cell_of(b"", ' '), "a missing glyph rendered as blank");
}

// ---------------------------------------------------------------- cursor

fn cursor_grid(focused: bool, visible: bool) -> Vec<u8> {
    let terminal = terminal(1, 3, b"abc\x1b[1;2H");
    let snapshot = Snapshot::new(&terminal, focused, false).with_cursor(Cursor {
        row: 0,
        column: 1,
        visible,
    });
    draw_grid(&snapshot).0
}

#[test]
fn a_focused_cursor_exchanges_its_cells_ink() {
    let hidden = cursor_grid(true, false);
    let shown = cursor_grid(true, true);
    let width = surface(1, 3).0;
    let (ink, paper) = (palette().foreground(), palette().background());
    for y in 0..face().height() {
        for x in 0..width {
            let under_cursor = (face().width()..face().width() * 2).contains(&x);
            let plain = rgb_at(&hidden, width, x, y);
            let drawn = rgb_at(&shown, width, x, y);
            if !under_cursor {
                assert_eq!(drawn, plain, "({x},{y}) outside the cursor");
            } else if plain == ink {
                assert_eq!(drawn, paper, "({x},{y}) glyph bit not exchanged");
            } else {
                assert_eq!(drawn, ink, "({x},{y}) background not exchanged");
            }
        }
    }
}

#[test]
fn an_unfocused_cursor_is_a_hollow_one_pixel_box() {
    let hidden = cursor_grid(false, false);
    let shown = cursor_grid(false, true);
    let width = surface(1, 3).0;
    let (cell_width, cell_height) = (face().width(), face().height());
    for y in 0..cell_height {
        for x in 0..width {
            let inside = (cell_width..cell_width * 2).contains(&x);
            let local = x.saturating_sub(cell_width);
            let edge = inside
                && (y == 0 || local == 0 || y + 1 == cell_height || local + 1 == cell_width);
            let drawn = rgb_at(&shown, width, x, y);
            if edge {
                assert_eq!(drawn, palette().foreground(), "({x},{y}) edge");
            } else {
                assert_eq!(drawn, rgb_at(&hidden, width, x, y), "({x},{y}) interior");
            }
        }
    }
}

#[test]
fn a_hidden_cursor_draws_nothing() {
    let terminal = terminal(1, 3, b"abc\x1b[?25l\x1b[1;2H");
    assert_eq!(terminal.mode("cursor-visible"), Some(false));
    let derived = Snapshot::new(&terminal, true, false);
    assert_eq!(derived.cursor(), None);
    assert_eq!(draw_grid(&derived).0, cursor_grid(true, false));
}

#[test]
fn a_focused_cursor_over_an_inverse_cell_reads_as_ordinary_text() {
    let inverse = terminal(1, 1, b"\x1b[7mA");
    let plain = terminal(1, 1, b"A");
    let over = Snapshot::new(&inverse, true, false).with_cursor(Cursor {
        row: 0,
        column: 0,
        visible: true,
    });
    let bare = Snapshot::new(&plain, false, false).with_cursor(Cursor {
        row: 0,
        column: 0,
        visible: false,
    });
    assert_eq!(draw_grid(&over).0, draw_grid(&bare).0);
}

#[test]
fn pending_wrap_does_not_move_the_drawn_cursor() {
    let terminal = terminal(1, 3, b"abc");
    let (row, column, pending) = terminal.cursor();
    assert!(pending, "the fixture did not reach pending wrap");
    assert_eq!((row, column), (0, 2));
    let snapshot = Snapshot::new(&terminal, true, false);
    assert_eq!(snapshot.cursor(), Some((0, 2)));
}

#[test]
fn a_cursor_outside_the_grid_is_not_drawn() {
    let terminal = terminal(1, 3, b"abc");
    let off_row = Snapshot::new(&terminal, true, false).with_cursor(Cursor {
        row: 4,
        column: 0,
        visible: true,
    });
    assert_eq!(off_row.cursor(), None);
    let off_column = Snapshot::new(&terminal, true, false).with_cursor(Cursor {
        row: 0,
        column: 9,
        visible: true,
    });
    assert_eq!(off_column.cursor(), None);
}

// ------------------------------------------------------------------ bell

#[test]
fn the_visual_bell_inverts_exactly_the_one_pixel_ring() {
    let terminal = terminal(2, 3, b"abcdef");
    let quiet = Snapshot::new(&terminal, false, false);
    let (quiet, width, height) = draw_grid(&quiet);
    let rung = Snapshot::new(&terminal, false, true);
    let rung = draw_grid(&rung).0;
    for y in 0..height {
        for x in 0..width {
            let ring = x == 0 || y == 0 || x + 1 == width || y + 1 == height;
            let [red, green, blue] = rgb_at(&quiet, width, x, y);
            let expected = if ring {
                [!red, !green, !blue]
            } else {
                [red, green, blue]
            };
            assert_eq!(rgb_at(&rung, width, x, y), expected, "({x},{y})");
        }
    }
}

#[test]
fn the_bell_ring_inverts_each_corner_exactly_once() {
    // A corner inverted twice would come back as it was, leaving holes.
    for (width, height) in [(1, 1), (1, 4), (4, 1), (2, 2), (3, 3)] {
        let mut pixels = vec![0; width * height * BYTES_PER_PIXEL];
        invert_ring(&mut pixels, width, height);
        let (chunks, _) = pixels.as_chunks::<BYTES_PER_PIXEL>();
        for (index, pixel) in chunks.iter().enumerate() {
            let (x, y) = (index % width, index / width);
            let ring = x == 0 || y == 0 || x + 1 == width || y + 1 == height;
            let expected = if ring { [255, 255, 255, 0] } else { [0, 0, 0, 0] };
            assert_eq!(*pixel, expected, "{width}x{height} ({x},{y})");
        }
    }
}

#[test]
fn the_bell_ring_leaves_the_unused_byte_zero() {
    let mut pixels = vec![0u8; 4 * 4 * BYTES_PER_PIXEL];
    invert_ring(&mut pixels, 4, 4);
    let (chunks, _) = pixels.as_chunks::<BYTES_PER_PIXEL>();
    assert!(chunks.iter().all(|pixel| pixel[3] == 0));
}

#[test]
fn an_empty_surface_rings_without_touching_anything() {
    let mut pixels = Vec::new();
    invert_ring(&mut pixels, 0, 0);
    assert!(pixels.is_empty());
}

// ------------------------------------------------------------- scrollback

/// Fill the history with `lines` distinguishable rows above a live screen.
fn scrolled(rows: usize, columns: usize, lines: usize) -> Terminal {
    let mut input = Vec::new();
    for line in 0..lines + rows {
        let digit = char::from_digit(u32::try_from(line % 10).unwrap(), 10).unwrap();
        for _ in 0..columns {
            input.push(u8::try_from(u32::from(digit)).unwrap());
        }
        if line + 1 < lines + rows {
            input.extend_from_slice(b"\r\n");
        }
    }
    terminal(rows, columns, &input)
}

#[test]
fn the_viewport_shows_history_above_the_live_screen() {
    let terminal = scrolled(2, 3, 3);
    assert_eq!(terminal.history_lines(), 3);
    assert_eq!(terminal.row_text(0).unwrap(), "333");
    assert_eq!(terminal.row_text(1).unwrap(), "444");

    let snapshot = Snapshot::new(&terminal, false, false).scrolled_back(1);
    assert_eq!(snapshot.viewport(), 1);
    // Row 0 is the newest history line, row 1 is the live screen's row 0.
    assert_eq!(snapshot.cell(0, 0).scalar, '2');
    assert_eq!(snapshot.cell(1, 0).scalar, '3');

    let snapshot = Snapshot::new(&terminal, false, false).scrolled_back(2);
    assert_eq!(snapshot.cell(0, 0).scalar, '1');
    assert_eq!(snapshot.cell(1, 0).scalar, '2');
}

#[test]
fn scrolled_back_clamps_to_the_available_history() {
    let deep = scrolled(2, 3, 3);
    let snapshot = Snapshot::new(&deep, false, false).scrolled_back(99);
    assert_eq!(snapshot.viewport(), 3);
    assert_eq!(snapshot.cell(0, 0).scalar, '0');
    let fresh = terminal(2, 3, b"ab");
    assert_eq!(
        Snapshot::new(&fresh, false, false)
            .scrolled_back(4)
            .viewport(),
        0
    );
}

/// The corpus can see the viewport's offset but not what it shows, and the
/// offset is exactly the number the anchor changes. So the property that
/// makes the anchor worth having is asserted here: the same lines stay on
/// screen while the child writes underneath them.
#[test]
fn an_anchored_view_shows_the_same_lines_while_output_arrives() {
    let mut terminal = scrolled(2, 3, 3);
    let mut viewport = keys::Viewport::new();
    let back = keys::Action::Scroll(keys::Scroll::Back);
    let seen = |terminal: &Terminal, viewport: &keys::Viewport| {
        let offset = viewport.offset(scrollback(terminal));
        let snapshot = Snapshot::new(terminal, false, false).scrolled_back(offset);
        (0..snapshot.rows())
            .map(|row| snapshot.cell(row, 0).scalar)
            .collect::<Vec<char>>()
    };

    viewport.apply(&back, terminal.rows(), scrollback(&terminal));
    let before = seen(&terminal, &viewport);
    assert_eq!(before, vec!['2', '3']);

    for line in 5..9 {
        let digit = char::from_digit(line % 10, 10).unwrap();
        let mut bytes = vec![b'\r', b'\n'];
        bytes.extend(std::iter::repeat_n(u8::try_from(u32::from(digit)).unwrap(), 3));
        terminal.feed(&bytes);
        assert_eq!(seen(&terminal, &viewport), before, "line {line} moved the view");
    }
    // It moved further from the bottom, which is the same thing said the
    // other way: four more lines arrived under it.
    assert_eq!(viewport.offset(scrollback(&terminal)), 5);
}

#[test]
fn the_viewport_pushes_the_cursor_down_and_then_off() {
    let terminal = scrolled(2, 3, 3);
    let live = Snapshot::new(&terminal, true, false);
    assert_eq!(live.cursor(), Some((1, 2)));
    let one = Snapshot::new(&terminal, true, false).scrolled_back(1);
    assert_eq!(one.cursor(), None, "row 1 + 1 is off a two-row grid");
    let zero = Snapshot::new(&terminal, true, false)
        .scrolled_back(1)
        .with_cursor(Cursor {
            row: 0,
            column: 1,
            visible: true,
        });
    assert_eq!(zero.cursor(), Some((1, 1)));
}

#[test]
fn an_open_viewport_reads_the_primary_screen_under_the_alternate() {
    let mut deep = scrolled(2, 3, 3);
    // A full-screen program takes the alternate screen and paints it.
    deep.feed(b"\x1b[?1049h\x1b[HXXX\r\nYYY");
    assert_eq!(deep.mode("alternate-screen"), Some(true));
    assert_eq!(deep.row_text(0).unwrap(), "XXX");

    // Closed, the viewport shows what the program drew.
    let live = Snapshot::new(&deep, false, false);
    assert_eq!(live.cell(0, 0).scalar, 'X');
    assert_eq!(live.cell(1, 0).scalar, 'Y');

    // Open, it is one coherent primary region: history above the split and
    // the PRIMARY screen below it, never the alternate rows.
    let back = Snapshot::new(&deep, false, false).scrolled_back(1);
    assert_eq!(back.cell(0, 0).scalar, '2', "history above the split");
    assert_eq!(back.cell(1, 0).scalar, '3', "primary screen below it");

    let deeper = Snapshot::new(&deep, false, false).scrolled_back(3);
    assert_eq!(deeper.cell(0, 0).scalar, '0');
    assert_eq!(deeper.cell(1, 0).scalar, '1');
}

#[test]
fn leaving_the_alternate_screen_leaves_the_viewport_unchanged() {
    let mut deep = scrolled(2, 3, 3);
    let before = Snapshot::new(&deep, false, false).scrolled_back(2);
    let before = (before.cell(0, 0).scalar, before.cell(1, 0).scalar);
    deep.feed(b"\x1b[?1049h\x1b[HXXX\r\nYYY\x1b[?1049l");
    assert_eq!(deep.mode("alternate-screen"), Some(false));
    let after = Snapshot::new(&deep, false, false).scrolled_back(2);
    assert_eq!((after.cell(0, 0).scalar, after.cell(1, 0).scalar), before);
}

#[test]
fn a_row_past_the_history_and_screen_is_blank() {
    let terminal = terminal(1, 2, b"ab");
    let snapshot = Snapshot::new(&terminal, false, false);
    assert_eq!(snapshot.cell(9, 0), BLANK);
    assert_eq!(snapshot.cell(0, 9), BLANK);
}

#[test]
fn a_history_line_shorter_than_the_grid_blanks_the_rest() {
    // The history stores each line at the width it scrolled off with, so a
    // widening resize leaves the tail of an old line unstored.
    let mut terminal = scrolled(2, 3, 2);
    terminal.resize(2, 6).unwrap();
    let snapshot = Snapshot::new(&terminal, false, false).scrolled_back(1);
    assert_eq!(snapshot.cell(0, 0).scalar, '1');
    assert_eq!(snapshot.cell(0, 5), BLANK);
}

#[test]
fn the_viewport_renders_history_pixels_rather_than_the_live_screen() {
    let terminal = scrolled(2, 3, 3);
    let live = draw_grid(&Snapshot::new(&terminal, false, false)).0;
    let back = draw_grid(&Snapshot::new(&terminal, false, false).scrolled_back(2)).0;
    assert_ne!(live, back);
}

// ------------------------------------------------------------------- ppm

#[test]
fn ppm_writes_the_exact_p6_header_and_rgb_triples() {
    let pixels = vec![1, 2, 3, 0, 4, 5, 6, 0];
    let encoded = ppm(&pixels, 2, 1).unwrap();
    assert_eq!(encoded, b"P6\n2 1\n255\n\x03\x02\x01\x06\x05\x04");
}

#[test]
fn ppm_round_trips_through_from_ppm() {
    let terminal = terminal(2, 4, b"\x1b[31mhi\x1b[0m\r\nyo");
    let (pixels, width, height) = draw_grid(&Snapshot::new(&terminal, true, true));
    let encoded = ppm(&pixels, width, height).unwrap();
    assert_eq!(from_ppm(&encoded).unwrap(), (pixels, width, height));
}

#[test]
fn ppm_rejects_a_surface_whose_length_is_not_its_area() {
    let error = ppm(&[0, 0, 0, 0], 2, 1).unwrap_err();
    assert!(error.contains("needs 8 bytes, not 4"), "{error}");
    let error = ppm(&[], usize::MAX, usize::MAX).unwrap_err();
    assert!(error.contains("overflows a byte count"), "{error}");
}

#[test]
fn from_ppm_rejects_every_malformed_header() {
    for (bytes, expected) in [
        (b"P3\n1 1\n255\n\0\0\0".as_slice(), "not P6"),
        (b"P6\n1 1\n254\n\0\0\0", "maxval is not 255"),
        (b"P6\nx 1\n255\n\0\0\0", "ppm width"),
        (b"P6\n1 y\n255\n\0\0\0", "ppm height"),
        (b"P6\n1 1\n255\n\0\0", "payload bytes"),
        (b"P6\n1 1\n255\n\0\0\0\0", "payload bytes"),
        (b"P6\n1 1\n255", "unterminated"),
        (b"   ", "ended before its header"),
        // A CRLF header would leave its `\n` at the head of the payload,
        // decoding to the right size with every pixel shifted one byte.
        (b"P6\r\n1 1\r\n255\r\n\0\0\0", "CRLF"),
    ] {
        let error = from_ppm(bytes).unwrap_err();
        assert!(error.contains(expected), "{expected:?} not in {error:?}");
    }
}

#[test]
fn from_ppm_keeps_a_payload_whose_first_byte_is_whitespace() {
    // Exactly one whitespace byte follows the maxval; a pixel that happens
    // to be 0x20 belongs to the image, not to the header.
    let (pixels, width, height) = from_ppm(b"P6\n1 1\n255\n\x20\x20\x20").unwrap();
    assert_eq!((width, height), (1, 1));
    assert_eq!(pixels, vec![0x20, 0x20, 0x20, 0]);
}

// ---------------------------------------------------------------- goldens

const GOLDEN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/spec/render");
/// Section 14's "beneath the build's temporary output": the crate's own
/// target directory, so parallel worktrees cannot collide on it and no
/// shared `/tmp` name has to be trusted.
const DIFF_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/render-diff");

/// The first differing coordinate and a high-contrast diff image: white
/// where the frames disagree, black where they agree. Split out of
/// `assert_golden` because it only ever runs on failure, which is the worst
/// time to discover it is wrong -- so it has its own cases.
fn diff_frames(pixels: &[u8], wanted: &[u8], width: usize) -> (Option<(usize, usize)>, Vec<u8>) {
    let mut diff = Vec::with_capacity(pixels.len());
    let mut first = None;
    for (index, offset) in (0..pixels.len()).step_by(BYTES_PER_PIXEL).enumerate() {
        let (a, b) = (
            pixels.get(offset..offset + BYTES_PER_PIXEL),
            wanted.get(offset..offset + BYTES_PER_PIXEL),
        );
        let same = a == b;
        if !same && first.is_none() && width != 0 {
            first = Some((index % width, index / width));
        }
        diff.extend_from_slice(if same { &[0, 0, 0, 0] } else { &[255, 255, 255, 0] });
    }
    (first, diff)
}

/// Section 14's oracle: compare against the committed image, and on a
/// mismatch report the first differing coordinate and leave the actual
/// frame plus a high-contrast diff under the build's temporary output.
fn assert_golden(name: &str, expected: &[u8], pixels: &[u8], width: usize, height: usize) {
    let actual = ppm(pixels, width, height).unwrap();
    if actual == expected {
        return;
    }
    let out = PathBuf::from(DIFF_DIR);
    std::fs::create_dir_all(&out).unwrap();
    let write = |suffix: &str, bytes: &[u8]| -> PathBuf {
        let path = out.join(format!("{name}.{suffix}.ppm"));
        std::fs::write(&path, bytes).unwrap();
        path
    };
    let actual_path = write("actual", &actual);

    let Ok((wanted, wanted_width, wanted_height)) = from_ppm(expected) else {
        panic!(
            "golden {name}.ppm is not readable P6; actual written to {}",
            actual_path.display()
        );
    };
    assert_eq!(
        (wanted_width, wanted_height),
        (width, height),
        "golden {name} is {wanted_width}x{wanted_height}, rendered {width}x{height}; \
         actual written to {}",
        actual_path.display()
    );
    assert_ne!(
        wanted,
        pixels,
        "golden {name} decodes to identical pixels, so its bytes are not the \
         canonical encoding `ppm` emits; actual written to {}",
        actual_path.display()
    );
    let (first, diff) = diff_frames(pixels, &wanted, width);
    let diff_path = write("diff", &ppm(&diff, width, height).unwrap());
    panic!(
        "golden {name} first differs at {:?}; actual {} diff {}",
        first,
        actual_path.display(),
        diff_path.display()
    );
}

macro_rules! golden {
    ($test:ident, $name:literal, $body:expr) => {
        #[test]
        fn $test() {
            let (pixels, width, height) = $body;
            assert_golden(
                $name,
                include_bytes!(concat!("../spec/render/", $name, ".ppm")),
                &pixels,
                width,
                height,
            );
        }
    };
}

fn hidden(terminal: &Terminal, focused: bool, bell: bool) -> Snapshot<'_> {
    Snapshot::new(terminal, focused, bell).with_cursor(Cursor {
        row: 0,
        column: 0,
        visible: false,
    })
}

golden!(golden_renditions, "renditions", {
    let terminal = terminal(
        1,
        7,
        b"A\x1b[1mA\x1b[0;2mA\x1b[0;3mA\x1b[0;4mA\x1b[0;9mA\x1b[0;7mA",
    );
    draw_grid(&hidden(&terminal, false, false))
});

golden!(golden_colors, "colors", {
    let terminal = terminal(
        1,
        4,
        b"\x1b[31mR\x1b[42mG\x1b[0;38;5;208mO\x1b[0;48;2;17;34;51mB",
    );
    draw_grid(&hidden(&terminal, false, false))
});

golden!(golden_cursor_focused, "cursor-focused", {
    let terminal = terminal(1, 3, b"abc\x1b[1;2H");
    draw_grid(&Snapshot::new(&terminal, true, false).with_cursor(Cursor {
        row: 0,
        column: 1,
        visible: true,
    }))
});

golden!(golden_cursor_unfocused, "cursor-unfocused", {
    let terminal = terminal(1, 3, b"abc\x1b[1;2H");
    draw_grid(&Snapshot::new(&terminal, false, false).with_cursor(Cursor {
        row: 0,
        column: 1,
        visible: true,
    }))
});

golden!(golden_bell, "bell", {
    let terminal = terminal(1, 3, b"abc");
    draw_grid(&hidden(&terminal, false, true))
});

golden!(golden_scrollback, "scrollback", {
    let terminal = scrolled(2, 3, 3);
    draw_grid(&hidden(&terminal, false, false).scrolled_back(2))
});

const GOLDENS: &[&str] = &[
    "bell",
    "colors",
    "cursor-focused",
    "cursor-unfocused",
    "renditions",
    "scrollback",
];

#[test]
fn the_committed_goldens_are_exactly_the_ones_a_case_renders() {
    // Read the directory rather than the roster: a `.ppm` left behind by a
    // renamed case still decodes and still passes every other check here,
    // and nothing else would ever look at it again.
    let mut found: Vec<String> = std::fs::read_dir(GOLDEN_DIR)
        .unwrap_or_else(|error| panic!("read {GOLDEN_DIR}: {error}"))
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    let expected: Vec<String> = GOLDENS.iter().map(|name| format!("{name}.ppm")).collect();
    assert_eq!(found, expected);
}

#[test]
fn every_golden_decodes_to_the_image_its_header_claims() {
    for name in GOLDENS {
        let path = PathBuf::from(GOLDEN_DIR).join(format!("{name}.ppm"));
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let (pixels, width, height) = from_ppm(&bytes)
            .unwrap_or_else(|error| panic!("decode {}: {error}", path.display()));
        assert_eq!(pixels.len(), width * height * BYTES_PER_PIXEL);
        assert!(width > 0 && height > 0);
        // The cell grid divides the surface exactly, so a golden cropped or
        // padded by a stray row would not be a frame this renderer emits.
        assert_eq!(width % face().width(), 0, "{name} width");
        assert_eq!(height % face().height(), 0, "{name} height");
    }
}

#[test]
fn the_diff_reports_the_first_differing_coordinate_in_row_major_order() {
    let width = 3;
    let mut pixels = vec![0u8; width * 2 * BYTES_PER_PIXEL];
    let wanted = pixels.clone();
    assert_eq!(diff_frames(&pixels, &wanted, width), (None, wanted.clone()));

    // Row 1, column 2 -- the last pixel -- and row 1 column 0 before it.
    *pixels.get_mut(5 * BYTES_PER_PIXEL).unwrap() = 9;
    *pixels.get_mut(3 * BYTES_PER_PIXEL).unwrap() = 9;
    let (first, diff) = diff_frames(&pixels, &wanted, width);
    assert_eq!(first, Some((0, 1)), "reported the later pixel first");
    let (chunks, _) = diff.as_chunks::<BYTES_PER_PIXEL>();
    let lit: Vec<usize> = chunks
        .iter()
        .enumerate()
        .filter(|(_, pixel)| **pixel == [255, 255, 255, 0])
        .map(|(index, _)| index)
        .collect();
    assert_eq!(lit, vec![3, 5]);
    assert!(chunks.iter().all(|pixel| pixel[3] == 0));
}

#[test]
fn the_diff_marks_a_truncated_frame_rather_than_reading_past_it() {
    // A golden shorter than the render must not silently compare equal.
    let pixels = vec![0u8; 2 * BYTES_PER_PIXEL];
    let (first, diff) = diff_frames(&pixels, &[], 2);
    assert_eq!(first, Some((0, 0)));
    assert_eq!(diff.len(), pixels.len());
    assert!(diff.as_chunks::<BYTES_PER_PIXEL>().0.iter().all(|pixel| *pixel == [255, 255, 255, 0]));
}

// -------------------------------------------------------------- selftest

#[test]
fn selftest_renders_the_pinned_face_and_round_trips_through_p6() {
    super::selftest().unwrap();
}
