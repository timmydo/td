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
    step, Bar, Block, Item, List, Panel, Row, Status, Strip, DISABLED, SELECTED_ROW, STATUS_COLUMNS,
};
use td_ui::font;
use td_ui::raster::{
    Draw, Primitive, Raster, Rect, Scale, Surface, Weight, BORDER, CHROME, INK, LINE_NUMBER, PAPER,
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
            Primitive::Glyph { .. } => None,
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
            Primitive::Fill { .. } => None,
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
