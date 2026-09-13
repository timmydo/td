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
    step, Bar, Block, Panel, Row, Status, Strip, DISABLED, SELECTED_ROW, STATUS_COLUMNS,
};
use td_ui::font;
use td_ui::raster::{
    Draw, Primitive, Raster, Rect, Scale, Surface, Weight, BORDER, CHROME, INK, PAPER,
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
