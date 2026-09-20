#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! Draw-stream and pixel oracles for the chrome bands: the menu bar with its
//! panel, the wrapped text block, the tab strip and the status row. Each band
//! streams its fills and glyphs inside a damage rectangle; the oracles pin
//! where the fills land, at which cells the glyphs sit and in which colour,
//! and paint one band whole to a buffer to confirm the pixels.

use td_ui::chrome::{
    step, Bar, Block, Field, Item, List, Panel, Row, Slider, Status, Strip, TextEntry,
    BUTTON_MARGIN, DISABLED, KNOB_WIDTH, SELECTED_ROW, STATUS_COLUMNS,
};
use td_ui::font;
use td_ui::raster::{
    Draw, Primitive, Raster, Rect, Scale, Surface, Weight, BORDER, CHROME, INACTIVE_SELECTION, INK,
    LINE_NUMBER, PAPER, SELECTED,
};

const LABELS: [&str; 5] = ["File", "Edit", "Format", "Help", "Directory"];

fn surface(width: usize, height: usize, scale: u8) -> Surface {
    Surface::new(width, height, Scale::new(scale).unwrap()).unwrap()
}

/// The fills and glyphs a band emits over the whole surface.
fn run(surface: Surface, emit: impl FnOnce(Rect, &mut dyn FnMut(Draw))) -> Vec<Draw> {
    let mut out = Vec::new();
    emit(surface.bounds(), &mut |draw| out.push(draw));
    out
}

fn fills(draws: &[Draw]) -> Vec<(Rect, u32)> {
    draws
        .iter()
        .filter_map(|d| match d.primitive {
            Primitive::Fill { rect, color } => Some((rect, color)),
            Primitive::Glyph { .. } | Primitive::Mark { .. } => None,
        })
        .collect()
}

/// Each mark as (x, y, scalar, ink).
fn marks(draws: &[Draw]) -> Vec<(i64, i64, char, u32)> {
    draws
        .iter()
        .filter_map(|d| match d.primitive {
            Primitive::Mark { x, y, scalar, ink } => Some((x, y, scalar, ink)),
            Primitive::Fill { .. } | Primitive::Glyph { .. } => None,
        })
        .collect()
}

/// Each glyph as (x, y, scalar, ink, weight).
fn glyphs(draws: &[Draw]) -> Vec<(i64, i64, char, u32, Weight)> {
    draws
        .iter()
        .filter_map(|d| match d.primitive {
            Primitive::Glyph {
                x,
                y,
                scalar,
                style,
            } => Some((x, y, scalar, style.ink, style.weight)),
            Primitive::Fill { .. } | Primitive::Mark { .. } => None,
        })
        .collect()
}

#[test]
fn the_menu_bar_fills_its_row_and_lays_the_labels_from_column_one() {
    for scale in 1..=4u8 {
        let s = scale as usize;
        let surface = surface(400 * s, 200, scale);
        let bar = Bar::new(surface, &LABELS);
        let draws = run(surface, |damage, sink| bar.emit(damage, sink));
        // One CHROME fill, the first row across the surface.
        assert_eq!(
            fills(&draws),
            vec![(
                Rect {
                    x: 0,
                    y: 0,
                    width: (400 * s) as u32,
                    height: (24 * s) as u32,
                },
                CHROME,
            )]
        );
        // The labels joined by three spaces, from cell column one, medium ink.
        let joined: String = LABELS.join("   ");
        let glyphs = glyphs(&draws);
        assert_eq!(glyphs.len(), joined.chars().count());
        for (index, (glyph, scalar)) in glyphs.iter().zip(joined.chars()).enumerate() {
            assert_eq!(
                *glyph,
                (
                    (8 + index * 8) as i64 * s as i64,
                    (4 * s) as i64,
                    scalar,
                    INK,
                    Weight::Medium,
                )
            );
        }
        // Header geometry answers the hit at its own corners and nothing below.
        for index in 0..LABELS.len() {
            let header = bar.header(index).unwrap();
            assert_eq!(bar.hit(header.x, header.y), Some(index));
            assert_eq!(
                bar.hit(header.x + i64::from(header.width) - 1, header.y),
                Some(index)
            );
            assert_eq!(bar.hit(header.x, (24 * s) as i64), None);
        }
        assert!(bar.header(LABELS.len()).is_none());
        // Help ends where Directory begins: from cell one, File, Edit and
        // Format each carry three trailing cells, and Help ends at cell 31.
        let help = bar.header(3).unwrap();
        assert_eq!(help.x + i64::from(help.width), 31 * 8 * s as i64);
    }
}

#[test]
fn a_menu_panel_paints_selection_disabled_ink_checks_and_right_aligned_shortcuts() {
    let s = 2usize;
    let surface = surface(320 * s, 600, 2);
    let header = Bar::new(surface, &LABELS).header(0).unwrap();
    let rows = [
        Row {
            label: "New",
            shortcut: "Ctrl+N",
            enabled: true,
            checked: false,
        },
        Row {
            label: "Save",
            shortcut: "Ctrl+S",
            enabled: false,
            checked: false,
        },
        Row {
            label: "Wrap",
            shortcut: "",
            enabled: true,
            checked: true,
        },
    ];
    let panel = Panel::new(surface, header, rows.len()).unwrap();
    let draws = run(surface, |damage, sink| {
        panel.emit(rows, 1, damage, sink);
    });
    let fills = fills(&draws);
    // One fill per row, the selected row highlighted and the others chrome.
    assert_eq!(fills.len(), rows.len());
    assert_eq!(fills[0].1, CHROME);
    assert_eq!(fills[1].1, SELECTED_ROW, "the selected row");
    assert_eq!(fills[2].1, CHROME);
    for (index, (rect, _)) in fills.iter().enumerate() {
        assert_eq!(rect.y, panel.rect().y + (index * 24 * s) as i64);
        assert_eq!(rect.height, (24 * s) as u32);
    }
    let glyphs = glyphs(&draws);
    // The disabled row's ink is dim; the others are full.
    let disabled: Vec<_> = glyphs.iter().filter(|g| g.3 == DISABLED).collect();
    assert!(!disabled.is_empty() && disabled.iter().all(|g| g.1 == fills[1].0.y + 4 * s as i64));
    // The checked row carries a leading "+ "; an unchecked one two spaces.
    let row2_y = panel.rect().y + (2 * 24 * s) as i64 + 4 * s as i64;
    let row2: String = glyphs
        .iter()
        .filter(|g| g.1 == row2_y && g.0 < (160 * s) as i64)
        .map(|g| g.2)
        .collect();
    assert!(row2.starts_with("+ Wrap"), "{row2:?}");
    // The shortcut sits at the row's right edge: its last cell is one cell in.
    let shortcut_end = glyphs
        .iter()
        .filter(|g| g.1 == panel.rect().y + 4 * s as i64)
        .map(|g| g.0)
        .max()
        .unwrap();
    assert_eq!(
        shortcut_end,
        panel.rect().x + i64::from(panel.rect().width) - (2 * 8 * s) as i64,
        "last shortcut cell one cell from the edge"
    );
}

#[test]
fn stepping_over_panel_rows_skips_disabled_and_wraps() {
    let enabled = |i: usize| i == 0 || i == 2;
    assert_eq!(step(3, 0, false, enabled), 2);
    assert_eq!(step(3, 2, false, enabled), 0);
    assert_eq!(step(3, 0, true, enabled), 2);
    assert_eq!(step(3, 0, false, |_| false), 0);
}

#[test]
fn the_tab_strip_paints_the_active_tab_paper_and_keeps_it_visible() {
    let s = 1usize;
    // Two tabs fit; a third is scrolled out unless it is active.
    let surface = surface(320 * s, 200, 1);
    let strip = Strip::new(surface, (24 * s) as i64, 0, 3).unwrap();
    let draws = run(surface, |damage, sink| {
        strip.emit(
            [("one", false), ("two", true), ("three", false)],
            damage,
            sink,
        );
    });
    let fills = fills(&draws);
    // The row fill, then per visible tab: background, top border, right border.
    assert_eq!(fills[0].1, CHROME, "the strip row");
    assert_eq!(fills[1].1, PAPER, "tab 0 active");
    assert_eq!(fills[2].1, BORDER);
    assert_eq!(fills[3].1, BORDER);
    assert_eq!(fills[4].1, CHROME, "tab 1 inactive");
    // Only two tabs are visible when the active one is first.
    assert!(strip.tab(2).is_none());
    // The dirty second tab carries a leading star.
    let glyphs = glyphs(&draws);
    let tab1 = strip.tab(1).unwrap();
    let tab1_text: String = glyphs
        .iter()
        .filter(|g| g.0 >= tab1.x && g.0 < tab1.x + i64::from(tab1.width) - (24 * s) as i64)
        .map(|g| g.2)
        .collect();
    assert_eq!(tab1_text, "*two");
    // With the last tab active it is kept in the strip and the first drops out.
    let strip = Strip::new(surface, (24 * s) as i64, 2, 3).unwrap();
    assert!(strip.tab(0).is_none());
    assert!(strip.tab(2).is_some());
}

#[test]
fn an_empty_strip_still_fills_its_row_and_paints_no_tab() {
    // A scene with no tabs still owns the strip's row: it stays chrome, not
    // the paper behind it.
    let surface = surface(320, 200, 1);
    let empty = Strip::new(surface, 24, 0, 0).unwrap();
    let draws = run(surface, |damage, sink| {
        empty.emit(std::iter::empty::<(&str, bool)>(), damage, sink);
    });
    assert_eq!(fills(&draws), vec![(empty.rect(), CHROME)]);
    assert!(glyphs(&draws).is_empty());
    assert!(empty.tab(0).is_none());
}

#[test]
fn a_text_block_wraps_at_its_columns_and_honours_newlines() {
    let s = 1usize;
    let surface = surface(11 * 8 * s, 200, 1); // nine content columns.
    let block = Block::new(surface, (24 * s) as i64, 3).unwrap();
    assert_eq!(block.columns(), 9);
    let draws = run(surface, |damage, sink| {
        block.emit("abcdefghijk\nx", damage, sink);
    });
    // One CHROME fill for the block, then the glyphs.
    assert_eq!(fills(&draws), vec![(block.rect(), CHROME)]);
    let wrapped = glyphs(&draws);
    let rows: Vec<i64> = {
        let mut ys: Vec<i64> = wrapped.iter().map(|g| g.1).collect();
        ys.dedup();
        ys
    };
    // Eleven scalars wrap after nine; the newline starts the third row.
    assert_eq!(rows.len(), 3);
    let row_text =
        |row: i64| -> String { wrapped.iter().filter(|g| g.1 == row).map(|g| g.2).collect() };
    assert_eq!(row_text(block.rect().y), "abcdefghi");
    assert_eq!(row_text(block.rect().y + (16 * s) as i64), "jk");
    assert_eq!(row_text(block.rect().y + (32 * s) as i64), "x");
    // Rows beyond the block are not painted.
    let short = Block::new(surface, (24 * s) as i64, 1).unwrap();
    let short_draws = run(surface, |damage, sink| {
        short.emit("abcdefghijk", damage, sink)
    });
    assert!(glyphs(&short_draws).iter().all(|g| g.1 == short.rect().y));
    assert!(Block::new(surface, 0, 16).is_none(), "past the row cap");
}

#[test]
fn the_status_row_shows_whole_cells_blanks_controls_and_marks_truncation() {
    let shown = |width: usize, line: &str| -> String {
        let surface = surface(width, 160, 1);
        let status = Status::new(surface);
        let draws = run(surface, |damage, sink| {
            status.emit(line.chars(), damage, sink)
        });
        // The row is chrome under a one-pixel top border.
        let fills = fills(&draws);
        assert_eq!(fills[0], (status.rect(), CHROME));
        assert_eq!(fills[1].1, BORDER);
        assert_eq!(fills[1].0.height, 1);
        glyphs(&draws).iter().map(|g| g.2).collect()
    };
    assert_eq!(shown(80, "Paste completed."), "Paste c…");
    assert_eq!(shown(80, "12345678"), "12345678");
    assert_eq!(shown(80, "hi\nx\ty"), "hi x y");
    assert_eq!(
        shown(8192, &"e".repeat(STATUS_COLUMNS)),
        "e".repeat(STATUS_COLUMNS)
    );
    assert_eq!(
        shown(8192, &"e".repeat(STATUS_COLUMNS + 1)),
        format!("{}…", "e".repeat(STATUS_COLUMNS - 1))
    );
}

#[test]
fn a_band_paints_its_pixels_and_leaves_the_rest_of_the_surface_alone() {
    let font = font::pinned().unwrap();
    let (width, height) = (320, 96);
    let surface = surface(width, height, 1);
    let status = Status::new(surface);
    let mut pixels = vec![0u8; width * height * 4];
    let mut raster = Raster::new(&mut pixels, &font, surface, width * 4).unwrap();
    status.emit("ok".chars(), surface.bounds(), &mut |draw| {
        raster.draw(draw)
    });
    let rect = status.rect();
    let pixel = |x: usize, y: usize| -> u32 {
        let base = (y * width + x) * 4;
        u32::from_le_bytes([
            pixels[base],
            pixels[base + 1],
            pixels[base + 2],
            pixels[base + 3],
        ])
    };
    // The top border row is BORDER, the fill below it CHROME, and a row above
    // the status is untouched (still zero).
    let y = rect.y as usize;
    assert_eq!(pixel(40, y) & 0xff_ffff, BORDER);
    assert_eq!(pixel(300, y + 4) & 0xff_ffff, CHROME);
    assert_eq!(pixel(40, y - 1), 0, "above the status stays untouched");
}

// ---- the paged list ----

fn items<'a>(specs: &'a [(&'a str, &'a str, bool, bool)]) -> Vec<Item<'a>> {
    specs
        .iter()
        .map(|&(label, meta, enabled, marked)| Item {
            label,
            meta,
            enabled,
            marked,
        })
        .collect()
}

#[test]
fn a_list_highlights_the_selection_dims_the_disabled_and_marks_and_right_aligns() {
    let surface = surface(320, 200, 1);
    let list = List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 120,
        },
    )
    .unwrap();
    assert_eq!(list.rows(), 5);
    let rows = items(&[
        ("alpha", "", true, false),
        ("bravo", "", true, false),     // selected
        ("charlie", "", true, true),    // marked
        ("delta", "off", false, false), // disabled, with a meta column
        ("echo", "", true, false),
    ]);
    let draws = run(surface, |damage, sink| {
        list.emit(rows, 0, 1, 5, damage, sink)
    });
    let filled = fills(&draws);
    let painted = glyphs(&draws);
    // Row 1 is the selection; row 0 is ordinary chrome.
    assert!(filled.contains(&(
        Rect {
            x: 0,
            y: 24,
            width: 304,
            height: 24
        },
        SELECTED_ROW
    )));
    assert!(filled.contains(&(
        Rect {
            x: 0,
            y: 0,
            width: 304,
            height: 24
        },
        CHROME
    )));
    // The whole rect is painted chrome first, covering the gutter and its
    // margin; with nothing to scroll the thumb is a border block.
    assert!(filled.contains(&(list.rect(), CHROME)));
    assert!(filled.iter().any(|&(_, c)| c == BORDER));
    // The marked row is prefixed with a star at its first cell.
    assert!(painted
        .iter()
        .any(|&(x, y, ch, _, _)| x == 8 && y == 2 * 24 + 4 && ch == '*'));
    // The selection paints ordinary ink; a disabled row is dim.
    assert!(painted
        .iter()
        .any(|&(_, y, ch, ink, _)| y == 24 + 4 && ch == 'b' && ink == INK));
    assert!(painted
        .iter()
        .any(|&(_, y, ch, ink, _)| y == 3 * 24 + 4 && ch == 'd' && ink == DISABLED));
    // The meta column is right-aligned within the body: "off" ends at its edge.
    assert!(painted
        .iter()
        .any(|&(x, y, ch, _, _)| x == 304 - (8 + 3 * 8) && y == 3 * 24 + 4 && ch == 'o'));
}

#[test]
fn a_short_list_fills_the_whole_rect_with_chrome_behind_its_rows() {
    let surface = surface(320, 200, 1);
    let list = List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 120,
        },
    )
    .unwrap();
    let rows = items(&[("one", "", true, false), ("two", "", true, false)]);
    let draws = run(surface, |damage, sink| {
        list.emit(rows, 0, 0, 2, damage, sink)
    });
    // The whole rect is one chrome fill, so the empty rows below the two
    // items — and the gutter and its margin — show chrome.
    assert!(fills(&draws).contains(&(list.rect(), CHROME)));
    // No glyph is painted below the last item.
    assert!(glyphs(&draws).iter().all(|&(_, y, _, _, _)| y < 48));
}

#[test]
fn reveal_moves_the_window_the_least_to_show_the_selection() {
    let surface = surface(320, 200, 1);
    let list = List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 120,
        },
    )
    .unwrap();
    assert_eq!(list.reveal(20, 0, 10), 0); // above the window: to its top
    assert_eq!(list.reveal(20, 19, 0), 15); // below: the last page
    assert_eq!(list.reveal(20, 3, 0), 0); // already shown: unchanged
    assert_eq!(list.reveal(20, 7, 0), 3); // just past: scroll by the overshoot
    assert_eq!(list.reveal(0, 0, 0), 0); // an empty list
}

#[test]
fn the_scrollbar_thumb_tracks_the_window() {
    let surface = surface(320, 200, 1);
    let list = List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 120,
        },
    )
    .unwrap();
    let top = list.scrollbar(20, 0);
    assert!(top.enabled());
    assert_eq!(top.thumb.y, top.track.y); // at the top
    assert!(top.thumb.height < top.track.height); // shorter than the track
    let bottom = list.scrollbar(20, 15);
    assert!(bottom.thumb.y > top.thumb.y); // scrolled down
}

#[test]
fn the_list_maps_points_to_visible_rows_and_ignores_the_gutter() {
    let surface = surface(320, 200, 1);
    let list = List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 120,
        },
    )
    .unwrap();
    assert_eq!(list.hit(10, 0), Some(0));
    assert_eq!(list.hit(10, 25), Some(1));
    assert_eq!(list.hit(10, 119), Some(4));
    assert_eq!(list.hit(10, 120), None); // below the rows
    assert_eq!(list.hit(310, 10), None); // in the scrollbar gutter
}

#[test]
fn a_list_paints_its_selection_and_scrollbar_to_pixels() {
    let font = font::pinned().unwrap();
    // A surface larger than the list, so the pixels around it must stay 0.
    let (width, height) = (360usize, 128usize);
    let surface = surface(width, height, 1);
    let list = List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 96,
        },
    )
    .unwrap();
    let rows = items(&[
        ("a", "", true, false),
        ("b", "", true, false), // selected
        ("c", "", true, false),
        ("d", "", true, false),
    ]);
    let mut pixels = vec![0u8; width * height * 4];
    let mut raster = Raster::new(&mut pixels, &font, surface, width * 4).unwrap();
    list.emit(rows, 0, 1, 20, surface.bounds(), &mut |draw| {
        raster.draw(draw)
    });
    let pixel = |x: usize, y: usize| -> u32 {
        let base = (y * width + x) * 4;
        u32::from_le_bytes([
            pixels[base],
            pixels[base + 1],
            pixels[base + 2],
            pixels[base + 3],
        ])
    };
    // Row 1 is the selection; row 0 stays chrome.
    assert_eq!(pixel(200, 24 + 8) & 0xff_ffff, SELECTED_ROW & 0xff_ffff);
    assert_eq!(pixel(200, 8) & 0xff_ffff, CHROME);
    // The selection paints its label ink over the highlight.
    let selection_has_ink = (24..48).any(|y| (0..304).any(|x| pixel(x, y) & 0xff_ffff == INK));
    assert!(selection_has_ink, "the selection row paints label ink");
    // The scrollbar thumb is line-number ink because twenty items scroll.
    assert_eq!(pixel(308, 8) & 0xff_ffff, LINE_NUMBER);
    // Below the thumb the track is chrome.
    assert_eq!(pixel(308, 90) & 0xff_ffff, CHROME);
    // The 4px margin past the 12px track is chrome now, not stale.
    assert_eq!(pixel(318, 8) & 0xff_ffff, CHROME);
    // The list leaves the rest of the surface untouched.
    assert_eq!(pixel(340, 8), 0, "right of the list stays untouched");
    assert_eq!(pixel(8, 100), 0, "below the list stays untouched");
}

#[test]
fn a_list_at_scale_two_and_an_offset_places_its_rows_and_gutter() {
    let surface = surface(400, 300, 2);
    let rect = Rect {
        x: 16,
        y: 16,
        width: 320,
        height: 144,
    };
    let list = List::new(surface, rect).unwrap();
    // 3 = 144 / (24 * 2); body drops the 32px gutter, a row is 48px tall.
    assert_eq!(list.rows(), 3);
    assert_eq!(
        list.body(),
        Rect {
            x: 16,
            y: 16,
            width: 288,
            height: 144
        }
    );
    assert_eq!(
        list.row(1),
        Some(Rect {
            x: 16,
            y: 64,
            width: 288,
            height: 48
        })
    );
    assert_eq!(list.row(3), None);
    let rows = items(&[
        ("one", "", true, false),
        ("two", "", true, false), // selected
        ("three", "", true, false),
    ]);
    let draws = run(surface, |damage, sink| {
        list.emit(rows, 0, 1, 3, damage, sink)
    });
    let filled = fills(&draws);
    // The whole rect is chrome; row 1 is the selection at the scaled offset.
    assert!(filled.contains(&(rect, CHROME)));
    assert!(filled.contains(&(
        Rect {
            x: 16,
            y: 64,
            width: 288,
            height: 48
        },
        SELECTED_ROW
    )));
    // The first label's 'o' sits two scaled cells in, past the "  " prefix.
    assert!(glyphs(&draws)
        .iter()
        .any(|&(x, y, ch, _, _)| x == 64 && y == 24 && ch == 'o'));
}

#[test]
fn a_selection_off_the_window_draws_no_highlight() {
    let surface = surface(320, 200, 1);
    let list = List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 72,
        },
    )
    .unwrap();
    assert_eq!(list.rows(), 3);
    // Window first=5 shows items 5..8; item 6 is the middle shown row.
    let shown = run(surface, |damage, sink| {
        list.emit(
            items(&[
                ("f", "", true, false),
                ("g", "", true, false),
                ("h", "", true, false),
            ]),
            5,
            6,
            20,
            damage,
            sink,
        )
    });
    assert!(fills(&shown).contains(&(
        Rect {
            x: 0,
            y: 24,
            width: 304,
            height: 24
        },
        SELECTED_ROW
    )));
    // A selection above the window paints no highlight at all.
    let off = run(surface, |damage, sink| {
        list.emit(
            items(&[
                ("f", "", true, false),
                ("g", "", true, false),
                ("h", "", true, false),
            ]),
            5,
            1,
            20,
            damage,
            sink,
        )
    });
    assert!(fills(&off).iter().all(|&(_, c)| c != SELECTED_ROW));
}

#[test]
fn a_list_with_a_remainder_fills_the_bottom_and_reports_exact_body() {
    let font = font::pinned().unwrap();
    let (width, height) = (320usize, 121usize); // 5 rows of 24 plus a 1px remainder
    let surface = surface(width, height, 1);
    let list = List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 121,
        },
    )
    .unwrap();
    assert_eq!(list.rows(), 5);
    // body reports the exact row area, not the 1px dead strip below it.
    assert_eq!(
        list.body(),
        Rect {
            x: 0,
            y: 0,
            width: 304,
            height: 120
        }
    );
    let rows = items(&[("only", "", true, false)]);
    let mut pixels = vec![0u8; width * height * 4];
    let mut raster = Raster::new(&mut pixels, &font, surface, width * 4).unwrap();
    list.emit(rows, 0, 0, 1, surface.bounds(), &mut |draw| {
        raster.draw(draw)
    });
    let pixel = |x: usize, y: usize| -> u32 {
        let base = (y * width + x) * 4;
        u32::from_le_bytes([
            pixels[base],
            pixels[base + 1],
            pixels[base + 2],
            pixels[base + 3],
        ])
    };
    // The 1px remainder row at the bottom is chrome, not stale.
    assert_eq!(pixel(100, 120) & 0xff_ffff, CHROME);
    assert_eq!(pixel(318, 120) & 0xff_ffff, CHROME);
}

#[test]
fn an_empty_list_paints_only_chrome_and_a_border_thumb() {
    let surface = surface(320, 200, 1);
    let rect = Rect {
        x: 0,
        y: 0,
        width: 320,
        height: 120,
    };
    let list = List::new(surface, rect).unwrap();
    let draws = run(surface, |damage, sink| {
        list.emit(items(&[]), 0, 0, 0, damage, sink)
    });
    // The whole rect is chrome and the disabled bar shows a border thumb.
    assert!(fills(&draws).contains(&(rect, CHROME)));
    assert!(fills(&draws).iter().any(|&(_, c)| c == BORDER));
    assert!(glyphs(&draws).is_empty());
    assert!(!list.scrollbar(0, 0).enabled());
}

#[test]
fn list_new_refuses_a_rect_the_surface_or_the_gutter_cannot_hold() {
    let surface = surface(320, 200, 1);
    // A rect reaching past the surface, sideways or below.
    assert!(List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 321,
            height: 24
        }
    )
    .is_none());
    assert!(List::new(
        surface,
        Rect {
            x: 0,
            y: 190,
            width: 320,
            height: 24
        }
    )
    .is_none());
    // Too narrow to hold a row beside the 16px gutter.
    assert!(List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 16,
            height: 24
        }
    )
    .is_none());
    // Shorter than one row.
    assert!(List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 23
        }
    )
    .is_none());
    // The minimum that holds: one row and a cell beside the gutter.
    assert!(List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 17,
            height: 24
        }
    )
    .is_some());
}

#[test]
fn reveal_clamps_a_stale_first_and_a_selection_past_the_end() {
    let surface = surface(320, 200, 1);
    let list = List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 120,
        },
    )
    .unwrap();
    assert_eq!(list.rows(), 5);
    // Fewer items than rows: the window stays at the top.
    assert_eq!(list.reveal(3, 2, 0), 0);
    // A first past the last page is clamped before the selection check.
    assert_eq!(list.reveal(20, 17, 99), 15);
    // A selection past the end is clamped to the last item.
    assert_eq!(list.reveal(20, 99, 0), 15);
}

#[test]
fn the_scrollbar_disables_when_nothing_scrolls() {
    let surface = surface(320, 200, 1);
    let list = List::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 120,
        },
    )
    .unwrap();
    assert_eq!(list.rows(), 5);
    // Fewer items than rows, and none at all, both disable the bar.
    assert!(!list.scrollbar(5, 0).enabled());
    assert!(!list.scrollbar(0, 0).enabled());
    // More items than rows enables it.
    assert!(list.scrollbar(6, 0).enabled());
}

/// A focused field at its default: caret at the end, no selection, shown.
fn field(text: &str) -> Field<'_> {
    Field {
        text,
        placeholder: "",
        caret: text.chars().count(),
        anchor: None,
        first: 0,
        masked: false,
        focused: true,
        caret_visible: true,
    }
}

#[test]
fn a_text_entry_paints_paper_the_text_and_a_one_pixel_caret() {
    let surface = surface(320, 200, 1);
    let entry = TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 24,
        },
    )
    .unwrap();
    assert_eq!(entry.columns(), 38); // (320 - 16) / 8
    let draws = run(surface, |damage, sink| {
        entry.emit(field("hello"), damage, sink)
    });
    let filled = fills(&draws);
    let painted = glyphs(&draws);
    // The paper ground fills the whole field.
    assert!(filled.contains(&(entry.rect(), PAPER)));
    // The caret is a one-pixel ink column after the last character.
    assert!(filled.contains(&(
        Rect {
            x: 48,
            y: 4,
            width: 1,
            height: 16
        },
        INK
    )));
    // The characters sit one cell in, medium ink.
    assert!(painted
        .iter()
        .any(|&(x, y, ch, ink, _)| x == 8 && y == 4 && ch == 'h' && ink == INK));
    assert!(painted.iter().any(|&(x, _, ch, _, _)| x == 40 && ch == 'o'));
}

#[test]
fn a_masked_field_shows_the_mask_not_the_characters() {
    let surface = surface(320, 200, 1);
    let entry = TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 24,
        },
    )
    .unwrap();
    let draws = run(surface, |damage, sink| {
        entry.emit(
            Field {
                masked: true,
                ..field("secret")
            },
            damage,
            sink,
        )
    });
    let painted = glyphs(&draws);
    // Every glyph is the mask, and none is a character of "secret".
    assert_eq!(painted.len(), 6);
    assert!(painted.iter().all(|&(_, _, ch, _, _)| ch == '\u{2022}'));
    assert!(!painted
        .iter()
        .any(|&(_, _, ch, _, _)| "secret".contains(ch)));
    // The masks advance one cell each from the first.
    assert!(painted.iter().any(|&(x, _, _, _, _)| x == 8));
    assert!(painted.iter().any(|&(x, _, _, _, _)| x == 8 + 5 * 8));
}

#[test]
fn a_selection_fills_its_cells_and_flips_the_ink() {
    let surface = surface(320, 200, 1);
    let entry = TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 24,
        },
    )
    .unwrap();
    for (focused, bg, sel_ink) in [(true, SELECTED, PAPER), (false, INACTIVE_SELECTION, INK)] {
        let draws = run(surface, |damage, sink| {
            entry.emit(
                Field {
                    anchor: Some(1),
                    caret: 4,
                    focused,
                    ..field("abcdef")
                },
                damage,
                sink,
            )
        });
        let filled = fills(&draws);
        let painted = glyphs(&draws);
        // Columns 1..4 (b, c, d) are one selection fill.
        assert!(filled.contains(&(
            Rect {
                x: 16,
                y: 4,
                width: 24,
                height: 16
            },
            bg
        )));
        // The selected glyphs take the selection ink; the rest ordinary ink.
        assert!(painted
            .iter()
            .any(|&(x, _, ch, ink, _)| x == 16 && ch == 'b' && ink == sel_ink));
        assert!(painted
            .iter()
            .any(|&(x, _, ch, ink, _)| x == 8 && ch == 'a' && ink == INK));
        assert!(painted
            .iter()
            .any(|&(x, _, ch, ink, _)| x == 40 && ch == 'e' && ink == INK));
    }
}

#[test]
fn reveal_scrolls_the_least_to_show_the_caret() {
    let surface = surface(320, 200, 1);
    // A five-column field: width = two insets plus five cells.
    let entry = TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 56,
            height: 24,
        },
    )
    .unwrap();
    assert_eq!(entry.columns(), 5);
    assert_eq!(entry.reveal(20, 0, 10), 0); // caret left of the window
    assert_eq!(entry.reveal(20, 19, 0), 14); // caret past the right: least move
    assert_eq!(entry.reveal(20, 3, 0), 0); // already shown: unchanged
    assert_eq!(entry.reveal(5, 5, 0), 0); // caret at the end of an exactly-filling value: no scroll
    assert_eq!(entry.reveal(6, 6, 0), 1); // one longer: scroll by one, caret in the inset
    assert_eq!(entry.reveal(20, 20, 99), 15); // caret at the end, stale first clamped to the tail
    assert_eq!(entry.reveal(3, 3, 0), 0); // a short value at its end
    assert_eq!(
        entry.reveal(usize::MAX, usize::MAX, usize::MAX),
        usize::MAX - 5
    ); // no overflow
    assert_eq!(entry.reveal(0, 0, 0), 0); // an empty value
}

#[test]
fn the_field_maps_a_point_to_a_caret_column() {
    let surface = surface(320, 200, 1);
    let entry = TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 24,
        },
    )
    .unwrap();
    assert_eq!(entry.columns(), 38);
    assert_eq!(entry.hit(8, 4, 0, 10), Some(0)); // the first cell
    assert_eq!(entry.hit(20, 4, 0, 10), Some(1)); // (20 - 8) / 8
    assert_eq!(entry.hit(4, 4, 0, 10), Some(0)); // the left inset clamps to 0
    assert_eq!(entry.hit(300, 4, 0, 10), Some(10)); // inside but past the text clamps to len
    assert_eq!(entry.hit(20, 4, 3, 10), Some(4)); // first shifts the column
    assert_eq!(entry.hit(315, 4, 0, 99), Some(38)); // right inset: last shown column
    assert_eq!(entry.hit(20, 4, usize::MAX, 10), Some(10)); // a hostile first is safe
    assert_eq!(entry.hit(1000, 4, 0, 10), None); // right of the field
    assert_eq!(entry.hit(10, 30, 0, 10), None); // below the field

    // At scale 2 and an offset the mapping scales.
    let s2 = Surface::new(400, 200, Scale::new(2).unwrap()).unwrap();
    let big = TextEntry::new(
        s2,
        Rect {
            x: 16,
            y: 16,
            width: 240,
            height: 48,
        },
    )
    .unwrap();
    assert_eq!(big.columns(), 13);
    assert_eq!(big.hit(16 + 16, 24, 0, 20), Some(0)); // one scaled cell in
    assert_eq!(big.hit(16 + 16 + 32, 24, 0, 20), Some(2)); // two scaled cells over
    assert_eq!(big.hit(16, 16, 0, 20), Some(0)); // the left inset
    assert_eq!(big.hit(10, 24, 0, 20), None); // left of the field
}

#[test]
fn a_placeholder_shows_dim_only_when_empty() {
    let surface = surface(320, 200, 1);
    let entry = TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 24,
        },
    )
    .unwrap();
    // Empty: the placeholder shows in the dim line-number ink.
    let empty = run(surface, |damage, sink| {
        entry.emit(
            Field {
                placeholder: "name",
                ..field("")
            },
            damage,
            sink,
        )
    });
    let hint = glyphs(&empty);
    assert!(hint
        .iter()
        .any(|&(x, y, ch, ink, _)| x == 8 && y == 4 && ch == 'n' && ink == LINE_NUMBER));
    assert_eq!(hint.len(), 4);
    // The caret still shows at the start.
    assert!(fills(&empty).contains(&(
        Rect {
            x: 8,
            y: 4,
            width: 1,
            height: 16
        },
        INK
    )));
    // Non-empty: the placeholder is not drawn; the text is, in ordinary ink.
    let typed = run(surface, |damage, sink| {
        entry.emit(
            Field {
                placeholder: "name",
                ..field("bob")
            },
            damage,
            sink,
        )
    });
    let shown = glyphs(&typed);
    assert!(shown.iter().all(|&(_, _, _, ink, _)| ink == INK));
    assert!(shown.iter().any(|&(_, _, ch, _, _)| ch == 'b'));
}

#[test]
fn text_entry_new_refuses_a_rect_the_surface_or_the_insets_cannot_hold() {
    let surface = surface(320, 200, 1);
    assert!(TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 321,
            height: 24
        }
    )
    .is_none());
    assert!(TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 190,
            width: 320,
            height: 24
        }
    )
    .is_none());
    // Narrower than two insets plus one text cell (24px), or shorter than a row.
    assert!(TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 23,
            height: 24
        }
    )
    .is_none());
    assert!(TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 23
        }
    )
    .is_none());
    // The minimum that holds: one text cell between the insets on one row.
    let min = TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 24,
            height: 24,
        },
    )
    .unwrap();
    assert_eq!(min.columns(), 1);
    // The guard scales: at scale 2 the minimum width is 48 and a row is 48.
    let s2 = Surface::new(320, 200, Scale::new(2).unwrap()).unwrap();
    assert!(TextEntry::new(
        s2,
        Rect {
            x: 0,
            y: 0,
            width: 47,
            height: 48
        }
    )
    .is_none());
    assert!(TextEntry::new(
        s2,
        Rect {
            x: 0,
            y: 0,
            width: 48,
            height: 47
        }
    )
    .is_none());
    assert_eq!(
        TextEntry::new(
            s2,
            Rect {
                x: 0,
                y: 0,
                width: 48,
                height: 48
            }
        )
        .unwrap()
        .columns(),
        1
    );
}

#[test]
fn a_text_entry_at_scale_two_and_an_offset_places_its_text_and_caret() {
    let surface = surface(400, 200, 2);
    let rect = Rect {
        x: 16,
        y: 16,
        width: 240,
        height: 48,
    };
    let entry = TextEntry::new(surface, rect).unwrap();
    assert_eq!(entry.columns(), 13); // (240 - 2*16) / 16
    let draws = run(surface, |damage, sink| {
        entry.emit(
            Field {
                caret: 2,
                ..field("hi")
            },
            damage,
            sink,
        )
    });
    let filled = fills(&draws);
    let painted = glyphs(&draws);
    assert!(filled.contains(&(rect, PAPER)));
    // Text one scaled cell in (x=32), one scaled inset down (y=24).
    assert!(painted
        .iter()
        .any(|&(x, y, ch, _, _)| x == 32 && y == 24 && ch == 'h'));
    assert!(painted.iter().any(|&(x, _, ch, _, _)| x == 48 && ch == 'i'));
    // The caret after "hi" is two scaled pixels wide.
    assert!(filled.contains(&(
        Rect {
            x: 64,
            y: 24,
            width: 2,
            height: 32
        },
        INK
    )));
}

#[test]
fn a_field_paints_its_selection_caret_and_text_to_pixels() {
    let font = font::pinned().unwrap();
    let (width, height) = (360usize, 48usize);
    let surface = surface(width, height, 1);
    let entry = TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 24,
        },
    )
    .unwrap();
    let mut pixels = vec![0u8; width * height * 4];
    let mut raster = Raster::new(&mut pixels, &font, surface, width * 4).unwrap();
    entry.emit(
        Field {
            anchor: Some(0),
            caret: 3,
            ..field("abc")
        },
        surface.bounds(),
        &mut |draw| raster.draw(draw),
    );
    let pixel = |x: usize, y: usize| -> u32 {
        let base = (y * width + x) * 4;
        u32::from_le_bytes([
            pixels[base],
            pixels[base + 1],
            pixels[base + 2],
            pixels[base + 3],
        ])
    };
    // The selection over "abc" shows the selection ground with paper ink.
    let has = |c: u32| (4..20).any(|y| (8..32).any(|x| pixel(x, y) & 0xff_ffff == c));
    assert!(has(SELECTED), "the selection ground");
    assert!(has(PAPER), "the selected glyph ink");
    // The caret after "abc" is a clean ink column with no glyph under it.
    assert_eq!(pixel(32, 12) & 0xff_ffff, INK);
    // The field ground past the text is paper.
    assert_eq!(pixel(300, 12) & 0xff_ffff, PAPER);
    // The field leaves the rest of the surface untouched.
    assert_eq!(pixel(340, 12), 0, "right of the field stays untouched");
    assert_eq!(pixel(8, 30), 0, "below the field stays untouched");
}

#[test]
fn a_masked_field_renders_the_mask_glyph_to_pixels() {
    let font = font::pinned().unwrap();
    let (width, height) = (320usize, 24usize);
    let surface = surface(width, height, 1);
    let entry = TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 24,
        },
    )
    .unwrap();
    // Render one field's value to a buffer.
    let paint = |text: &str, masked: bool| -> Vec<u8> {
        let mut pixels = vec![0u8; width * height * 4];
        let mut raster = Raster::new(&mut pixels, &font, surface, width * 4).unwrap();
        entry.emit(
            Field {
                masked,
                caret_visible: false,
                ..field(text)
            },
            surface.bounds(),
            &mut |draw| raster.draw(draw),
        );
        pixels
    };
    let masked_x = paint("x", true);
    // Masking "x" paints exactly the bullet, not the letter.
    assert_eq!(
        masked_x,
        paint("\u{2022}", false),
        "masked 'x' renders the bullet"
    );
    assert_ne!(masked_x, paint("x", false), "the bullet differs from 'x'");
    // And the bullet is a real glyph, not blank.
    let ink = (4..20).any(|y| {
        (8..16).any(|x| {
            let base = (y * width + x) * 4;
            u32::from_le_bytes([
                masked_x[base],
                masked_x[base + 1],
                masked_x[base + 2],
                masked_x[base + 3],
            ]) & 0xff_ffff
                == INK
        })
    });
    assert!(ink, "the mask glyph renders ink");
}

#[test]
fn a_scrolled_field_shows_its_window_and_places_the_caret_and_selection() {
    let surface = surface(320, 200, 1);
    // A five-column field scrolled to first = 3 over a ten-character value.
    let entry = TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 56,
            height: 24,
        },
    )
    .unwrap();
    assert_eq!(entry.columns(), 5);
    let draws = run(surface, |damage, sink| {
        entry.emit(
            Field {
                first: 3,
                anchor: Some(4),
                caret: 6,
                ..field("abcdefghij")
            },
            damage,
            sink,
        )
    });
    let filled = fills(&draws);
    let painted = glyphs(&draws);
    // The window shows exactly the five characters from column three.
    assert_eq!(painted.len(), 5);
    assert!(painted
        .iter()
        .all(|&(_, _, ch, _, _)| ch != 'a' && ch != 'c'));
    assert!(painted
        .iter()
        .any(|&(x, _, ch, ink, _)| x == 8 && ch == 'd' && ink == INK));
    assert!(painted
        .iter()
        .any(|&(x, _, ch, ink, _)| x == 16 && ch == 'e' && ink == PAPER));
    assert!(painted
        .iter()
        .any(|&(x, _, ch, ink, _)| x == 40 && ch == 'h' && ink == INK));
    // The selection over columns 4..6 sits shifted by first.
    assert!(filled.contains(&(
        Rect {
            x: 16,
            y: 4,
            width: 16,
            height: 16
        },
        SELECTED
    )));
    // The caret at column 6 is three cells into the window.
    assert!(filled.contains(&(
        Rect {
            x: 32,
            y: 4,
            width: 1,
            height: 16
        },
        INK
    )));
}

#[test]
fn an_unfocused_selection_paints_its_inactive_ground_to_pixels() {
    let font = font::pinned().unwrap();
    let (width, height) = (320usize, 24usize);
    let surface = surface(width, height, 1);
    let entry = TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 24,
        },
    )
    .unwrap();
    let mut pixels = vec![0u8; width * height * 4];
    let mut raster = Raster::new(&mut pixels, &font, surface, width * 4).unwrap();
    entry.emit(
        Field {
            anchor: Some(0),
            caret: 3,
            focused: false,
            caret_visible: false,
            ..field("abc")
        },
        surface.bounds(),
        &mut |draw| raster.draw(draw),
    );
    let pixel = |x: usize, y: usize| -> u32 {
        let base = (y * width + x) * 4;
        u32::from_le_bytes([
            pixels[base],
            pixels[base + 1],
            pixels[base + 2],
            pixels[base + 3],
        ])
    };
    // The unfocused selection is the inactive ground with ordinary ink over it.
    let has = |c: u32| (4..20).any(|y| (8..32).any(|x| pixel(x, y) & 0xff_ffff == c));
    assert!(has(INACTIVE_SELECTION), "the inactive selection ground");
    assert!(has(INK), "ordinary ink over the inactive ground");
}

#[test]
fn a_stale_first_cannot_panic_the_field_and_keeps_the_caret() {
    let surface = surface(320, 200, 1);
    let entry = TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 24,
        },
    )
    .unwrap();
    // An empty field with a first left over from a scrolled value still
    // shows the caret at the start.
    let empty = run(surface, |damage, sink| {
        entry.emit(
            Field {
                first: usize::MAX,
                ..field("")
            },
            damage,
            sink,
        )
    });
    assert!(fills(&empty).contains(&(
        Rect {
            x: 8,
            y: 4,
            width: 1,
            height: 16
        },
        INK
    )));
    // A non-empty field with a hostile first paints the paper and does not
    // panic (overflow-safe arithmetic).
    let stale = run(surface, |damage, sink| {
        entry.emit(
            Field {
                first: usize::MAX,
                caret: usize::MAX,
                anchor: Some(usize::MAX),
                ..field("abc")
            },
            damage,
            sink,
        )
    });
    assert!(fills(&stale).contains(&(entry.rect(), PAPER)));
}

#[test]
fn the_caret_shows_only_when_visible_and_inside_the_window() {
    let surface = surface(320, 200, 1);
    let entry = TextEntry::new(
        surface,
        Rect {
            x: 0,
            y: 0,
            width: 56,
            height: 24,
        },
    )
    .unwrap();
    assert_eq!(entry.columns(), 5);
    let carets = |f: Field| {
        let draws = run(surface, |damage, sink| entry.emit(f, damage, sink));
        fills(&draws)
            .into_iter()
            .filter(|&(r, c)| c == INK && r.width == 1)
            .count()
    };
    // Hidden when the caller says so.
    assert_eq!(
        carets(Field {
            caret_visible: false,
            ..field("abc")
        }),
        0
    );
    // Hidden when the caret is scrolled out of the window (caret 9, first 0).
    assert_eq!(
        carets(Field {
            caret: 9,
            first: 0,
            ..field("abcdefghij")
        }),
        0
    );
    // Shown when visible and inside the window.
    assert_eq!(
        carets(Field {
            caret: 2,
            ..field("abc")
        }),
        1
    );
}

#[test]
fn action_buttons_have_borders_shared_hit_bounds_and_clipped_focus_pixels() {
    let font = font::pinned().unwrap();
    for scale in 1..=4u8 {
        let s = usize::from(scale);
        let surface = surface(240 * s, 64 * s, scale);
        let rect = Rect {
            x: (8 * s) as i64,
            y: (8 * s) as i64,
            width: (176 * s) as u32,
            height: (24 * s) as u32,
        };
        let button = td_ui::chrome::Button::new(surface, rect).unwrap();
        assert!(button.hit(rect.x, rect.y));
        assert!(!button.hit(rect.x - 1, rect.y));
        assert!(!button.hit(rect.x + i64::from(rect.width), rect.y));
        for (selected, enabled) in [(false, true), (true, true), (true, false)] {
            let paint = |damage| {
                let mut pixels = vec![0x11; surface.width * surface.height * 4];
                let mut raster =
                    Raster::new(&mut pixels, &font, surface, surface.width * 4).unwrap();
                button.emit("Refresh: 1 s", selected, enabled, damage, &mut |draw| {
                    raster.draw(draw)
                });
                pixels
            };
            let full = paint(surface.bounds());
            let damage = Rect {
                x: rect.x + 3,
                y: rect.y + 2,
                width: rect.width / 2,
                height: rect.height / 2,
            };
            let partial = paint(damage);
            for y in 0..surface.height {
                for x in 0..surface.width {
                    let offset = (y * surface.width + x) * 4;
                    let pixel = &partial[offset..offset + 4];
                    if damage.contains(x as i64, y as i64) {
                        assert_eq!(pixel, &full[offset..offset + 4]);
                    } else {
                        assert_eq!(pixel, &[0x11; 4]);
                    }
                }
            }
            let corner = (rect.y as usize * surface.width + rect.x as usize) * 4;
            assert_eq!(
                &full[corner..corner + 4],
                &(BORDER | 0xff000000).to_le_bytes()
            );
            let draws = run(surface, |damage, sink| {
                button.emit("Action", selected, enabled, damage, sink)
            });
            assert!(glyphs(&draws).iter().all(|g| g.3
                == if !enabled {
                    DISABLED
                } else if selected {
                    PAPER
                } else {
                    INK
                }));
        }
        assert!(td_ui::chrome::Button::new(surface, Rect { x: -1, ..rect }).is_none());
        assert!(td_ui::chrome::Button::new(surface, Rect { width: 0, ..rect }).is_none());
        // The text is centred in the height: four pixels down in a row-tall
        // button, two in one four pixels shorter, and at the top of one a
        // cell tall.
        for (height, down) in [(24, 4), (20, 2), (16, 0)] {
            let short = Rect {
                height: (height * s) as u32,
                ..rect
            };
            let button = td_ui::chrome::Button::new(surface, short).unwrap();
            let draws = run(surface, |damage, sink| {
                button.emit("Action", false, true, damage, sink)
            });
            let ys: Vec<i64> = glyphs(&draws).iter().map(|g| g.1).collect();
            assert_eq!(ys, vec![short.y + (down * s) as i64; 6], "height {height}");
        }
    }
}

#[test]
fn a_button_strip_lays_bordered_buttons_from_cell_one_and_hits_only_whole_ones() {
    use td_ui::chrome::{Buttons, BUTTON_MARGIN, ROW};
    let font = font::pinned().unwrap();
    let labels = ["All", "Picks", "Rejects", "Unflagged"];
    for scale in 1..=4u8 {
        let s = usize::from(scale);
        let surface = surface(320 * s, 72 * s, scale);
        let y = (ROW * s) as i64;
        let strip = Buttons::new(surface, y, &labels);
        assert_eq!(
            strip.rect(),
            Rect {
                x: 0,
                y,
                width: (320 * s) as u32,
                height: (ROW * s) as u32
            }
        );
        // Each button is its label's cells and one each side, from cell
        // one, a cell between, inset by the margin above and below.
        let expected = |x: usize, columns: usize| Rect {
            x: (x * 8 * s) as i64,
            y: y + (BUTTON_MARGIN * s) as i64,
            width: (columns * 8 * s) as u32,
            height: ((ROW - 2 * BUTTON_MARGIN) * s) as u32,
        };
        let rects: Vec<Rect> = (0..4).map(|i| strip.button(i).unwrap().rect()).collect();
        assert_eq!(
            rects,
            vec![
                expected(1, 5),
                expected(7, 7),
                expected(15, 9),
                expected(25, 11)
            ]
        );
        assert!(strip.button(4).is_none());
        // A hit is a button's own pixels: not the gap, not the margin.
        let mid = y + (ROW * s / 2) as i64;
        assert_eq!(strip.hit(rects[1].x, mid), Some(1));
        assert_eq!(strip.hit(rects[1].x + i64::from(rects[1].width), mid), None);
        assert_eq!(strip.hit(rects[2].x - 1, mid), None);
        assert_eq!(strip.hit(rects[0].x, y), None);
        assert_eq!(strip.hit(rects[0].x, y + (ROW * s) as i64 - 1), None);
        assert_eq!(strip.hit(rects[3].x, mid), Some(3));
        // The band fills chrome, then each button its border, its face
        // and its label: the selected one dark with paper ink, the
        // disabled one paper with dim ink, the rest paper with ink; a
        // state the iterator ran out of is an enabled unselected button.
        let draws = run(surface, |damage, sink| {
            strip.emit([(false, true), (true, true), (false, false)], damage, sink)
        });
        let painted = fills(&draws);
        assert_eq!(painted[0], (strip.rect(), CHROME));
        assert_eq!(
            painted[1..].iter().map(|f| f.1).collect::<Vec<u32>>(),
            vec![BORDER, PAPER, BORDER, SELECTED, BORDER, PAPER, BORDER, PAPER]
        );
        let lettered = glyphs(&draws);
        let text: String = lettered.iter().map(|g| g.2).collect();
        assert_eq!(text, "AllPicksRejectsUnflagged");
        assert!(lettered
            .iter()
            .all(|g| g.1 == y + (BUTTON_MARGIN * s) as i64 + (2 * s) as i64));
        assert_eq!(lettered[0].0, rects[0].x + (8 * s) as i64);
        let inks: Vec<u32> = lettered.iter().map(|g| g.3).collect();
        assert_eq!(&inks[..3], &[INK; 3]);
        assert_eq!(&inks[3..8], &[PAPER; 5]);
        assert_eq!(&inks[8..15], &[DISABLED; 7]);
        assert_eq!(&inks[15..], &[INK; 9]);
        // The pixels: a border at a button's corner, chrome in the gap and
        // the margin, the selected face dark, all within the band.
        let mut pixels = vec![0x11; surface.width * surface.height * 4];
        let mut raster = Raster::new(&mut pixels, &font, surface, surface.width * 4).unwrap();
        strip.emit(
            [(false, true), (true, true)],
            surface.bounds(),
            &mut |draw| raster.draw(draw),
        );
        let pixel = |x: i64, y: i64| {
            let offset = (y as usize * surface.width + x as usize) * 4;
            u32::from_le_bytes([
                pixels[offset],
                pixels[offset + 1],
                pixels[offset + 2],
                pixels[offset + 3],
            ]) & 0xffffff
        };
        assert_eq!(pixel(rects[0].x, rects[0].y), BORDER);
        assert_eq!(pixel(rects[0].x + i64::from(rects[0].width), mid), CHROME);
        assert_eq!(pixel(rects[0].x, y), CHROME);
        assert_eq!(pixel(rects[0].x, y + (ROW * s) as i64 - 1), CHROME);
        assert_eq!(
            pixel(rects[1].x + (2 * s) as i64, rects[1].y + (2 * s) as i64),
            SELECTED
        );
        assert_eq!(pixel(0, y - 1), 0x111111);
        assert_eq!(pixel(0, y + (ROW * s) as i64), 0x111111);
        // A label of descenders keeps its lowest glyph row inside the
        // bezel: ink on the last row the glyphs cover, the row under it
        // the face, then the border.
        let low = ["gjpqy"];
        let strip_low = Buttons::new(surface, y, &low);
        let mut lowered = vec![0x11; surface.width * surface.height * 4];
        let mut raster = Raster::new(&mut lowered, &font, surface, surface.width * 4).unwrap();
        strip_low.emit([(false, true)], surface.bounds(), &mut |draw| {
            raster.draw(draw)
        });
        let low_rect = strip_low.button(0).unwrap().rect();
        let row_has = |row: i64, color: u32| {
            (low_rect.x..low_rect.x + i64::from(low_rect.width)).any(|x| {
                let offset = (row as usize * surface.width + x as usize) * 4;
                u32::from_le_bytes([
                    lowered[offset],
                    lowered[offset + 1],
                    lowered[offset + 2],
                    lowered[offset + 3],
                ]) & 0xffffff
                    == color
            })
        };
        let last_glyph_row = low_rect.y + (2 * s + 16 * s) as i64 - 1;
        assert!(row_has(last_glyph_row, INK), "scale {s}");
        assert!(!row_has(last_glyph_row + (s as i64), INK), "scale {s}");
        assert!(row_has(low_rect.y + i64::from(low_rect.height) - 1, BORDER));
        // Painting within a damage rectangle is the same pixels inside it
        // and nothing outside; damage off the band paints nothing.
        let paint = |damage| {
            let mut pixels = vec![0x11; surface.width * surface.height * 4];
            let mut raster = Raster::new(&mut pixels, &font, surface, surface.width * 4).unwrap();
            strip.emit([(false, true), (true, true)], damage, &mut |draw| {
                raster.draw(draw)
            });
            pixels
        };
        let full = paint(surface.bounds());
        let damage = Rect {
            x: rects[1].x - 3,
            y: y + 1,
            width: rects[1].width,
            height: (ROW * s) as u32 / 2,
        };
        let partial = paint(damage);
        for py in 0..surface.height {
            for px in 0..surface.width {
                let offset = (py * surface.width + px) * 4;
                if damage.contains(px as i64, py as i64) {
                    assert_eq!(&partial[offset..offset + 4], &full[offset..offset + 4]);
                } else {
                    assert_eq!(&partial[offset..offset + 4], &[0x11; 4]);
                }
            }
        }
        let off = Rect {
            x: 0,
            y: 0,
            width: surface.width as u32,
            height: y as u32,
        };
        let mut nothing = Vec::new();
        strip.emit([(false, true)], off, &mut |draw| nothing.push(draw));
        assert!(nothing.is_empty());
        // The strip is one row, its end after the last button on it.
        assert_eq!(strip.rows(), 1);
        assert_eq!(
            strip.end(),
            (rects[3].x + i64::from(rects[3].width) + (8 * s) as i64, y)
        );
        // A surface too narrow for the last button whole starts the next
        // row with it: the band is two rows, the button at cell one a row
        // down, painted and hit there; the end is after it on that row.
        let narrow = Surface::new(272 * s, 72 * s, Scale::new(scale).unwrap()).unwrap();
        let strip = Buttons::new(narrow, y, &labels);
        assert_eq!(strip.rows(), 2);
        assert_eq!(
            strip.rect(),
            Rect {
                x: 0,
                y,
                width: (272 * s) as u32,
                height: (2 * ROW * s) as u32
            }
        );
        let wrapped = Rect {
            y: y + (ROW * s) as i64 + (BUTTON_MARGIN * s) as i64,
            ..expected(1, 11)
        };
        assert_eq!(strip.button(3).map(|b| b.rect()), Some(wrapped));
        assert!(strip.button(2).is_some());
        assert_eq!(strip.hit(rects[3].x, mid), None);
        assert_eq!(strip.hit(wrapped.x, wrapped.y), Some(3));
        assert_eq!(
            strip.end(),
            (
                wrapped.x + i64::from(wrapped.width) + (8 * s) as i64,
                y + (ROW * s) as i64
            )
        );
        let draws = run(narrow, |damage, sink| {
            strip.emit(std::iter::repeat((false, true)), damage, sink)
        });
        assert_eq!(fills(&draws)[0], (strip.rect(), CHROME));
        assert_eq!(fills(&draws).len(), 1 + 2 * 4);
        let text: String = glyphs(&draws).iter().map(|g| g.2).collect();
        assert_eq!(text, "AllPicksRejectsUnflagged");
        // A band given its own left and width lays from there; a button
        // wider than a whole row is neither laid nor given room, the next
        // going on where it would have been.
        let band = Buttons::in_band(surface, (16 * s) as i64, y, (120 * s) as u32, &labels);
        let at = |x: usize, columns: usize, row: usize| Rect {
            x: ((16 + x * 8) * s) as i64,
            y: y + ((row * ROW + BUTTON_MARGIN) * s) as i64,
            width: (columns * 8 * s) as u32,
            height: ((ROW - 2 * BUTTON_MARGIN) * s) as u32,
        };
        assert_eq!(band.rows(), 3);
        assert_eq!(band.button(0).map(|b| b.rect()), Some(at(1, 5, 0)));
        assert_eq!(band.button(1).map(|b| b.rect()), Some(at(7, 7, 0)));
        assert_eq!(band.button(2).map(|b| b.rect()), Some(at(1, 9, 1)));
        // The third row runs under this surface's foot: counted, not held.
        assert!(band.button(3).is_none());
        assert_eq!(
            band.end(),
            (
                at(1, 11, 2).x + (11 * 8 * s) as i64 + (8 * s) as i64,
                y + (2 * ROW * s) as i64
            )
        );
        let wide = ["All", "Twenty-one characters", "Picks"];
        let band = Buttons::in_band(surface, (16 * s) as i64, y, (120 * s) as u32, &wide);
        assert_eq!(band.rows(), 1);
        assert!(band.button(1).is_none());
        assert_eq!(band.button(2).map(|b| b.rect()), Some(at(7, 7, 0)));
        // A band placed past the integer range has no buttons and no hit,
        // nor has one whose buttons would run under the surface's foot.
        let far = Buttons::new(surface, i64::MAX - 1, &labels);
        assert!(far.button(0).is_none() && far.hit(rects[0].x, i64::MAX - 1).is_none());
        let foot = (surface.height - (ROW - BUTTON_MARGIN) * s + 1) as i64;
        let low = Buttons::new(surface, foot, &labels);
        assert!(
            low.button(0).is_none() && low.hit(rects[0].x, foot + (ROW * s / 2) as i64).is_none()
        );
        let fits = Buttons::new(surface, foot - 1, &labels);
        assert!(fits.button(0).is_some());
        // An empty strip is its chrome band alone, one row, its end at
        // cell one.
        let empty = Buttons::new(surface, y, &[]);
        assert_eq!((empty.rows(), empty.end()), (1, ((8 * s) as i64, y)));
        let draws = run(surface, |damage, sink| {
            empty.emit(std::iter::empty(), damage, sink)
        });
        assert_eq!(fills(&draws), vec![(empty.rect(), CHROME)]);
        assert!(glyphs(&draws).is_empty() && empty.hit(rects[0].x, mid).is_none());
    }
}

/// A hinted button at every scale: the caption four pixels higher than
/// a plain one's, a band of the face over its descenders from seven
/// pixels above the foot, and the hint's marks centred under the caption
/// on that band in the lighter ink (the caption's own when selected or
/// disabled), from the face's left when the caption is the narrower, a
/// mark the bezel would cut left off; an empty hint is none.
#[test]
fn a_hinted_button_lifts_its_caption_and_marks_the_hint_under_it() {
    use td_ui::chrome::{Buttons, ROW};
    use td_ui::hint;
    let font = font::pinned().unwrap();
    let labels = ["All", "Picks", "Rejects", "-", "gjpqy"];
    for scale in 1..=4u8 {
        let s = usize::from(scale);
        let px = |n: usize| (n * s) as i64;
        let surface = surface(320 * s, 72 * s, scale);
        let y = (ROW * s) as i64;
        let strip = Buttons::new(surface, y, &labels);
        let rects: Vec<Rect> = (0..5).map(|i| strip.button(i).unwrap().rect()).collect();
        let states = [(false, true), (true, true), (false, false)];
        let hints = [
            Some("1"),
            Some("2"),
            Some("Backspace"),
            Some("Backspace"),
            Some("z"),
        ];
        let draws = run(surface, |damage, sink| {
            strip.emit_hinted(states, hints, damage, sink)
        });
        // A plain caption sits two pixels down; a hinted one two up.
        let lettered = glyphs(&draws);
        assert!(lettered.iter().all(|g| g.1 == rects[0].y - px(2)));
        // Each hinted button paints its band after its face, in the face's
        // colour, over the inner width from seven pixels above the foot.
        let painted = fills(&draws);
        let band = |rect: Rect, color| {
            (
                Rect {
                    x: rect.x + px(1),
                    y: rect.y + i64::from(rect.height) - px(7),
                    width: rect.width - 2 * px(1) as u32,
                    height: (hint::HEIGHT * s) as u32,
                },
                color,
            )
        };
        assert_eq!(
            painted[1..],
            [
                (rects[0], BORDER),
                (
                    Rect {
                        x: rects[0].x + px(1),
                        y: rects[0].y + px(1),
                        width: rects[0].width - 2 * px(1) as u32,
                        height: rects[0].height - 2 * px(1) as u32
                    },
                    PAPER
                ),
                band(rects[0], PAPER),
                (rects[1], BORDER),
                (
                    Rect {
                        x: rects[1].x + px(1),
                        y: rects[1].y + px(1),
                        width: rects[1].width - 2 * px(1) as u32,
                        height: rects[1].height - 2 * px(1) as u32
                    },
                    SELECTED
                ),
                band(rects[1], SELECTED),
                (rects[2], BORDER),
                (
                    Rect {
                        x: rects[2].x + px(1),
                        y: rects[2].y + px(1),
                        width: rects[2].width - 2 * px(1) as u32,
                        height: rects[2].height - 2 * px(1) as u32
                    },
                    PAPER
                ),
                band(rects[2], PAPER),
                (rects[3], BORDER),
                (
                    Rect {
                        x: rects[3].x + px(1),
                        y: rects[3].y + px(1),
                        width: rects[3].width - 2 * px(1) as u32,
                        height: rects[3].height - 2 * px(1) as u32
                    },
                    PAPER
                ),
                band(rects[3], PAPER),
                (rects[4], BORDER),
                (
                    Rect {
                        x: rects[4].x + px(1),
                        y: rects[4].y + px(1),
                        width: rects[4].width - 2 * px(1) as u32,
                        height: rects[4].height - 2 * px(1) as u32
                    },
                    PAPER
                ),
                band(rects[4], PAPER),
            ]
        );
        // The marks: centred under the caption, `hint::ADVANCE` apart, on
        // the band's rows; the lighter ink, or the caption's when the
        // button is selected or disabled; from the face's left, the marks
        // the bezel would cut left off, when the hint is wider than the
        // caption (`Back` of `Backspace`: 22 pixels hold four marks and
        // three spaces, not a fifth mark).
        let marked = marks(&draws);
        let hint_y = rects[0].y + i64::from(rects[0].height) - px(7);
        assert!(marked.iter().all(|m| m.1 == hint_y));
        let text: String = marked.iter().map(|m| m.2).collect();
        assert_eq!(text, "12BackspaceBackz");
        assert_eq!(
            marked[0].0,
            rects[0].x + px(8) + (px(24) - px(hint::width("1"))) / 2
        );
        assert_eq!(marked[0].3, LINE_NUMBER);
        assert_eq!(
            marked[1].0,
            rects[1].x + px(8) + (px(40) - px(hint::width("2"))) / 2
        );
        assert_eq!(marked[1].3, PAPER);
        let backspace = &marked[2..11];
        assert_eq!(
            backspace[0].0,
            rects[2].x + px(8) + (px(56) - px(hint::width("Backspace"))) / 2
        );
        assert!(backspace
            .iter()
            .enumerate()
            .all(|(i, m)| m.0 == backspace[0].0 + px(hint::ADVANCE) * i as i64 && m.3 == DISABLED));
        let cut = &marked[11..15];
        assert_eq!(cut[0].0, rects[3].x + px(1));
        assert_eq!(cut[3].0, rects[3].x + px(1) + px(3 * hint::ADVANCE));
        // The pixels: the hint's lit and unlit cells, the band's row under
        // it, the bezel, no caption ink on the band's rows and a
        // descender's first row above them.
        let mut pixels = vec![0x11; surface.width * surface.height * 4];
        let mut raster = Raster::new(&mut pixels, &font, surface, surface.width * 4).unwrap();
        strip.emit_hinted(states, hints, surface.bounds(), &mut |draw| {
            raster.draw(draw)
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
        // '1' is `.#..` over `##..`, `.#..`, `.#..`, `###.`.
        let one = marked[0].0;
        assert_eq!(pixel(one, hint_y), PAPER);
        assert_eq!(pixel(one + px(1), hint_y), LINE_NUMBER);
        assert_eq!(
            pixel(one + px(1) + px(1) - 1, hint_y + px(1) - 1),
            LINE_NUMBER
        );
        assert_eq!(pixel(one, hint_y + px(4)), LINE_NUMBER);
        assert_eq!(pixel(one + px(3), hint_y + px(4)), PAPER);
        assert_eq!(pixel(one, hint_y + px(5)), PAPER);
        assert_eq!(
            pixel(one, rects[0].y + i64::from(rects[0].height) - 1),
            BORDER
        );
        let row_has = |rect: Rect, row: i64, color: u32| {
            (rect.x..rect.x + i64::from(rect.width)).any(|x| pixel(x, row) == color)
        };
        for row in hint_y..rects[4].y + i64::from(rects[4].height) - px(1) {
            assert!(!row_has(rects[4], row, INK), "scale {s} row {row}");
        }
        assert!(row_has(rects[4], hint_y - 1, INK));
        assert!(row_has(rects[4], hint_y + px(1), LINE_NUMBER));
        // A plain strip is the same draws as one hinted with nothing, or
        // with empty hints.
        let plain = run(surface, |damage, sink| strip.emit(states, damage, sink));
        let none = run(surface, |damage, sink| {
            strip.emit_hinted(states, [None; 5], damage, sink)
        });
        assert_eq!(plain, none);
        let empty = run(surface, |damage, sink| {
            strip.emit_hinted(states, [Some(""); 5], damage, sink)
        });
        assert_eq!(plain, empty);
        assert!(marks(&plain).is_empty());
    }
}

/// The slider's geometry at every scale: the knob a button's height and
/// `KNOB_WIDTH` wide, travelling from the rectangle's left edge to a knob
/// short of its right; `value_at` puts the knob's centre under the pointer
/// and rounds to the nearest of `steps + 1` positions, the ends clamped.
#[test]
fn a_slider_lays_its_knob_along_the_travel_and_maps_the_pointer_to_the_nearest_step() {
    for scale in 1..=4u8 {
        let s = scale as usize;
        let surface = surface(400 * s, 72 * s, scale);
        let rect = Rect {
            x: (16 * s) as i64,
            y: (24 * s) as i64,
            width: (212 * s) as u32,
            height: (24 * s) as u32,
        };
        let slider = Slider::new(surface, rect).unwrap();
        assert_eq!(slider.rect(), rect);
        let knob = (KNOB_WIDTH * s) as i64;
        let span = i64::from(rect.width) - knob;
        let margin = (BUTTON_MARGIN * s) as i64;
        let at = |x: i64| Rect {
            x,
            y: rect.y + margin,
            width: knob as u32,
            height: (rect.height as i64 - 2 * margin) as u32,
        };
        // Ten steps over a 200-pixel travel land the knob every twenty
        // pixels, the last flush with the rectangle's right edge, and a
        // value past the last step is the last step.
        for value in 0..=10 {
            assert_eq!(
                slider.knob(value, 10),
                at(rect.x + 20 * s as i64 * value as i64)
            );
        }
        assert_eq!(slider.travel(), span as u32);
        assert_eq!(slider.knob(11, 10), slider.knob(10, 10));
        assert_eq!(slider.knob(usize::MAX, 10), slider.knob(10, 10));
        assert_eq!(slider.knob(0, 0), at(rect.x));
        assert_eq!(slider.knob(7, 0), at(rect.x));
        // A knob's own centre maps back to its value; a pointer between two
        // positions goes to the nearer, and beyond either end to that end.
        for value in 0..=10 {
            let centre = slider.knob(value, 10).x + knob / 2;
            assert_eq!(
                slider.value_at(centre, 10),
                value,
                "scale {scale} value {value}"
            );
            assert_eq!(slider.value_at(centre + 9 * s as i64, 10), value);
            if value < 10 {
                assert_eq!(slider.value_at(centre + 11 * s as i64, 10), value + 1);
            }
        }
        assert_eq!(slider.value_at(i64::MIN, 10), 0);
        assert_eq!(slider.value_at(i64::MAX, 10), 10);
        assert_eq!(slider.value_at(rect.x + rect.width as i64 + 1, 0), 0);
        // A travel of twelve pixels and ten steps still lands every
        // knob's centre back on its own value, both directions rounding;
        // more steps than pixels cannot, and the widget does not pretend.
        let narrow = Slider::new(
            surface,
            Rect {
                width: (24 * s) as u32,
                ..rect
            },
        )
        .unwrap();
        for value in 0..=10 {
            let centre = narrow.knob(value, 10).x + knob / 2;
            assert_eq!(narrow.value_at(centre, 10), value, "narrow {value}");
        }
        // Step one of ten over the twelve is at 1.2 scaled pixels, rounded.
        assert_eq!(narrow.knob(1, 10).x - rect.x, (12 * s as i64 + 5) / 10);
        let many = 13 * s;
        assert!((0..=many).any(|value| {
            let centre = narrow.knob(value, many).x + knob / 2;
            narrow.value_at(centre, many) != value
        }));
        // Hits are the whole rectangle, band included, and nothing outside.
        let (right, bottom) = (rect.x + 212 * s as i64, rect.y + 24 * s as i64);
        assert!(slider.hit(rect.x, rect.y) && slider.hit(right - 1, bottom - 1));
        assert!(!slider.hit(rect.x - 1, rect.y) && !slider.hit(right, rect.y));
        assert!(!slider.hit(rect.x, rect.y - 1) && !slider.hit(rect.x, bottom));
    }
}

/// The slider paints its chrome, its track, then the knob's bezel and face
/// in that order so the knob covers the track; the face is paper when
/// enabled and chrome when not; a damage rectangle clips every fill and one
/// outside the slider emits nothing; and the pixels of an enabled knob at
/// scale two show the bezel, the face and the track either side.
#[test]
fn a_slider_paints_track_then_bezelled_knob_and_clips_to_the_damage() {
    let surface = surface(400, 72, 1);
    let rect = Rect {
        x: 16,
        y: 24,
        width: 212,
        height: 24,
    };
    let slider = Slider::new(surface, rect).unwrap();
    let draws = run(surface, |damage, sink| {
        slider.emit(5, 10, true, damage, sink)
    });
    assert!(glyphs(&draws).is_empty());
    let knob = slider.knob(5, 10);
    assert_eq!(
        knob,
        Rect {
            x: 116,
            y: 26,
            width: 12,
            height: 20
        }
    );
    let track = Rect {
        x: 22,
        y: 36,
        width: 200,
        height: 1,
    };
    let face = Rect {
        x: 117,
        y: 27,
        width: 10,
        height: 18,
    };
    assert_eq!(
        fills(&draws),
        vec![
            (rect, CHROME),
            (track, BORDER),
            (knob, BORDER),
            (face, PAPER)
        ]
    );
    let disabled = run(surface, |damage, sink| {
        slider.emit(5, 10, false, damage, sink)
    });
    assert_eq!(fills(&disabled)[3], (face, CHROME));
    // Damage covering the knob's left half alone clips each fill to it.
    let part = Rect {
        x: 110,
        y: 24,
        width: 12,
        height: 24,
    };
    let mut clipped = Vec::new();
    slider.emit(5, 10, true, part, &mut |draw| clipped.push(draw));
    assert_eq!(
        fills(&clipped),
        vec![
            (
                Rect {
                    x: 110,
                    y: 24,
                    width: 12,
                    height: 24
                },
                CHROME
            ),
            (
                Rect {
                    x: 110,
                    y: 36,
                    width: 12,
                    height: 1
                },
                BORDER
            ),
            (
                Rect {
                    x: 116,
                    y: 26,
                    width: 6,
                    height: 20
                },
                BORDER
            ),
            (
                Rect {
                    x: 117,
                    y: 27,
                    width: 5,
                    height: 18
                },
                PAPER
            ),
        ]
    );
    let mut nothing = Vec::new();
    let off = Rect {
        x: 0,
        y: 0,
        width: 400,
        height: 24,
    };
    slider.emit(5, 10, true, off, &mut |draw| nothing.push(draw));
    assert!(nothing.is_empty());
    let mut outside = Vec::new();
    let far = Rect {
        x: 1000,
        y: 0,
        width: 4,
        height: 4,
    };
    slider.emit(5, 10, true, far, &mut |draw| outside.push(draw));
    assert!(outside.is_empty());
    // Pixels at scale two: a two-pixel bezel round a paper face, the track
    // two pixels thick through the knob's middle rows on either side of it,
    // and chrome above and below.
    let font = font::pinned().unwrap();
    let (width, height) = (400, 72);
    let two = Surface::new(width, height, Scale::new(2).unwrap()).unwrap();
    let rect = Rect {
        x: 0,
        y: 0,
        width: 400,
        height: 48,
    };
    let slider = Slider::new(two, rect).unwrap();
    let mut pixels = vec![0u8; width * height * 4];
    let mut raster = Raster::new(&mut pixels, &font, two, width * 4).unwrap();
    slider.emit(0, 1, true, two.bounds(), &mut |draw| raster.draw(draw));
    assert_eq!(
        slider.knob(0, 1),
        Rect {
            x: 0,
            y: 4,
            width: 24,
            height: 40
        }
    );
    let px = |x: usize, y: usize| -> u32 {
        let base = (y * width + x) * 4;
        u32::from_le_bytes([
            pixels[base],
            pixels[base + 1],
            pixels[base + 2],
            pixels[base + 3],
        ]) & 0xff_ffff
    };
    assert_eq!(px(0, 4), BORDER);
    assert_eq!(px(1, 5), BORDER);
    assert_eq!(px(2, 6), PAPER);
    assert_eq!(px(21, 41), PAPER);
    assert_eq!(px(23, 43), BORDER);
    assert_eq!(px(0, 3), CHROME);
    assert_eq!(px(0, 44), CHROME);
    assert_eq!(px(12, 24), PAPER);
    assert_eq!(px(30, 23), BORDER);
    assert_eq!(px(30, 24), BORDER);
    assert_eq!(px(30, 22), CHROME);
    assert_eq!(px(30, 25), CHROME);
    assert_eq!(px(387, 24), BORDER);
    assert_eq!(px(388, 24), CHROME);
    // The track starts under the knob's centre, so the knob covers its
    // left end; just past the knob it shows.
    assert_eq!(px(24, 24), BORDER);
    assert_eq!(px(24, 22), CHROME);
    assert_eq!(px(200, 60), 0, "below the slider stays untouched");
}

/// `new` refuses a rectangle the surface cannot hold, one narrower than two
/// knobs or one shorter than its margins, and a surface that fails its own
/// check.
#[test]
fn slider_new_refuses_a_rect_the_surface_or_the_knob_cannot_hold() {
    let surface = surface(200, 48, 2);
    let ok = Rect {
        x: 0,
        y: 0,
        width: 48,
        height: 14,
    };
    assert!(Slider::new(surface, ok).is_some());
    for bad in [
        Rect { width: 47, ..ok },
        Rect { height: 13, ..ok },
        Rect { x: 153, ..ok },
        Rect { y: 35, ..ok },
        Rect { x: -1, ..ok },
        Rect {
            width: 0,
            height: 0,
            ..ok
        },
    ] {
        assert!(Slider::new(surface, bad).is_none(), "{bad:?}");
    }
    assert!(Slider::new(surface, Rect { x: 152, ..ok }).is_some());
    assert!(Slider::new(surface, Rect { y: 34, ..ok }).is_some());
    // The shortest slider still shows a bezel round a pixel of face.
    let short = Slider::new(surface, ok).unwrap();
    let draws = run(surface, |damage, sink| {
        short.emit(0, 1, false, damage, sink)
    });
    let knob = short.knob(0, 1);
    assert_eq!(knob.height, 6);
    assert_eq!(
        fills(&draws)[3],
        (
            Rect {
                x: knob.x + 2,
                y: knob.y + 2,
                width: knob.width - 4,
                height: 2
            },
            CHROME
        )
    );
    assert!(Slider::new(
        Surface {
            width: 0,
            ..surface
        },
        ok
    )
    .is_none());
}
