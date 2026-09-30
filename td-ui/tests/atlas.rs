#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The atlas page over masks built by hand: placement without overlap and
//! with its gutter zeroed, the copied coverage, the dirty band, blank and
//! missing slots, idempotent placement, the reset on a full page and on a
//! spent key budget, and the epoch that marks it.

use td_ui::atlas::{Atlas, Entry, Slot, Style, MAX_KEYS, PAGE_HEIGHT, PAGE_WIDTH};
use td_ui::coverage::Mask;

fn mask(width: usize, height: usize, seed: u8) -> Mask {
    Mask {
        width,
        height,
        left: -1,
        top: 7,
        alpha: (0..width * height)
            .map(|i| seed.wrapping_add(i as u8) | 1)
            .collect(),
    }
}

fn entry(slot: Slot) -> Entry {
    match slot {
        Slot::Placed(entry) => entry,
        other => panic!("not placed: {other:?}"),
    }
}

fn copied(atlas: &Atlas, entry: Entry, mask: &Mask) -> bool {
    (0..entry.height).all(|y| {
        let row = (entry.y + y) * PAGE_WIDTH + entry.x;
        atlas.page()[row..row + entry.width] == mask.alpha[y * mask.width..(y + 1) * mask.width]
    })
}

#[test]
fn masks_are_copied_without_overlap_and_remembered() {
    let mut atlas = Atlas::new();
    assert_eq!(atlas.page().len(), PAGE_WIDTH * PAGE_HEIGHT);
    assert!(atlas.is_empty());
    let masks = [
        mask(10, 20, 1),
        mask(7, 19, 2),
        mask(30, 40, 3),
        mask(10, 20, 4),
    ];
    let mut entries = vec![];
    for (index, m) in masks.iter().enumerate() {
        let scalar = char::from(b'a' + index as u8);
        let placed = entry(atlas.place(Style::Regular, scalar, m));
        assert_eq!(
            (placed.width, placed.height, placed.left, placed.top),
            (m.width, m.height, -1, 7)
        );
        assert!(copied(&atlas, placed, m));
        assert_eq!(
            atlas.get(Style::Regular, scalar),
            Some(Slot::Placed(placed))
        );
        entries.push(placed);
    }
    // Pairwise apart, with a pixel of gutter between neighbours.
    for (i, a) in entries.iter().enumerate() {
        for b in &entries[i + 1..] {
            let apart = a.x + a.width < b.x
                || b.x + b.width < a.x
                || a.y + a.height < b.y
                || b.y + b.height < a.y;
            assert!(apart, "{a:?} {b:?}");
        }
    }
    // Every mask copied intact after the others landed.
    for (placed, m) in entries.iter().zip(&masks) {
        assert!(copied(&atlas, *placed, m));
    }
    // Styles are separate keys; a shorter mask shares a close shelf.
    assert_eq!(atlas.get(Style::Bold, 'a'), None);
    assert_eq!(
        entries[1].y, entries[0].y,
        "a 19-row mask on the 20-row shelf"
    );
    assert_ne!(
        entries[2].y, entries[0].y,
        "a 40-row mask gets its own shelf"
    );
    assert_eq!(atlas.len(), 4);
    assert_eq!(atlas.epoch(), 0);
}

#[test]
fn the_dirty_band_covers_what_was_written_since_taken() {
    let mut atlas = Atlas::new();
    assert_eq!(atlas.take_dirty(), None);
    let first = entry(atlas.place(Style::Regular, 'a', &mask(4, 10, 0)));
    let second = entry(atlas.place(Style::Regular, 'b', &mask(4, 30, 0)));
    assert_eq!(atlas.take_dirty(), Some((first.y, second.y + 30 + 1)));
    assert_eq!(atlas.take_dirty(), None);
    atlas.record(Style::Regular, 'c', Slot::Missing);
    assert_eq!(
        atlas.take_dirty(),
        None,
        "a slot without coverage writes no rows"
    );
    let third = entry(atlas.place(Style::Regular, 'd', &mask(4, 10, 0)));
    assert_eq!(atlas.take_dirty(), Some((third.y, third.y + 10 + 1)));
}

#[test]
fn empty_and_oversized_masks_take_no_space() {
    let mut atlas = Atlas::new();
    assert_eq!(
        atlas.place(Style::Regular, ' ', &Mask::default()),
        Slot::Blank
    );
    assert_eq!(atlas.get(Style::Regular, ' '), Some(Slot::Blank));
    let wide = mask(PAGE_WIDTH, 1, 0);
    assert_eq!(atlas.place(Style::Regular, 'w', &wide), Slot::Missing);
    let tall = mask(1, PAGE_HEIGHT, 0);
    assert_eq!(atlas.place(Style::Bold, 't', &tall), Slot::Missing);
    assert_eq!(atlas.take_dirty(), None);
    let fits = mask(PAGE_WIDTH - 1, PAGE_HEIGHT - 1, 5);
    let placed = entry(atlas.place(Style::Regular, 'f', &fits));
    assert_eq!((placed.x, placed.y), (0, 0));
    assert!(copied(&atlas, placed, &fits));
}

#[test]
fn a_full_page_resets_under_a_new_epoch() {
    let mut atlas = Atlas::new();
    let big = mask(500, 300, 9);
    let mut scalar = 'A';
    let mut placed = 0;
    while atlas.epoch() == 0 {
        let slot = atlas.place(Style::Regular, scalar, &big);
        assert!(matches!(slot, Slot::Placed(_)));
        scalar = char::from_u32(u32::from(scalar) + 1).unwrap();
        placed += 1;
    }
    // Two a shelf, three shelves: the seventh reset the page.
    assert_eq!(placed, 7);
    assert_eq!(atlas.epoch(), 1);
    assert_eq!(atlas.len(), 1, "only the mask that did not fit survives");
    assert_eq!(atlas.get(Style::Regular, 'A'), None);
    let last = char::from_u32(u32::from(scalar) - 1).unwrap();
    let survivor = entry(atlas.get(Style::Regular, last).unwrap());
    assert_eq!((survivor.x, survivor.y), (0, 0));
    assert!(copied(&atlas, survivor, &big));
}

#[test]
fn a_spent_key_budget_resets_before_recording() {
    let mut atlas = Atlas::new();
    for code in 0..MAX_KEYS as u32 {
        atlas.record(
            Style::Regular,
            char::from_u32(0x4e00 + code).unwrap(),
            Slot::Missing,
        );
    }
    assert_eq!((atlas.len(), atlas.epoch()), (MAX_KEYS, 0));
    // Re-recording a held key spends nothing.
    atlas.record(Style::Regular, '\u{4e00}', Slot::Missing);
    assert_eq!((atlas.len(), atlas.epoch()), (MAX_KEYS, 0));
    atlas.record(Style::Regular, 'x', Slot::Blank);
    assert_eq!((atlas.len(), atlas.epoch()), (1, 1));
    for code in 0..MAX_KEYS as u32 - 1 {
        atlas.record(
            Style::Bold,
            char::from_u32(0x4e00 + code).unwrap(),
            Slot::Missing,
        );
    }
    let placed = atlas.place(Style::Regular, 'y', &mask(3, 3, 0));
    assert!(matches!(placed, Slot::Placed(_)));
    assert_eq!((atlas.len(), atlas.epoch()), (1, 2));
    atlas.reset();
    assert!(atlas.is_empty());
    assert_eq!(atlas.epoch(), 3);
}

#[test]
fn a_new_epoch_zeroes_each_entry_gutter_and_repeats_are_idempotent() {
    let mut atlas = Atlas::new();
    let full = Mask {
        width: 100,
        height: 100,
        left: 0,
        top: 0,
        alpha: vec![255; 100 * 100],
    };
    let mut code = 0x100;
    while atlas.epoch() == 0 {
        atlas.place(Style::Regular, char::from_u32(code).unwrap(), &full);
        code += 1;
    }
    // Every page byte an earlier entry left is 255; the new small entry's
    // right column and bottom row of gutter must read zero.
    let small = mask(50, 50, 3);
    let placed = entry(atlas.place(Style::Bold, 'x', &small));
    assert!(copied(&atlas, placed, &small));
    for y in placed.y..=placed.y + 50 {
        assert_eq!(
            atlas.page()[y * PAGE_WIDTH + placed.x + 50],
            0,
            "column, row {y}"
        );
    }
    let below = (placed.y + 50) * PAGE_WIDTH + placed.x;
    assert!(atlas.page()[below..below + 51].iter().all(|&a| a == 0));

    // A held key keeps its slot and takes no second region.
    let again = atlas.place(Style::Bold, 'x', &mask(7, 7, 9));
    assert_eq!(again, Slot::Placed(placed));
    // Alpha that is not width by height is never copied.
    let short = Mask {
        alpha: vec![1; 10],
        ..mask(4, 4, 0)
    };
    assert_eq!(atlas.place(Style::Regular, 's', &short), Slot::Missing);
}
