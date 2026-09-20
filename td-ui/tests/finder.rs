#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The directory finder's oracles: the listing's bounds, the filter's
//! rule, navigation and reveal, the outcomes of choosing in either mode,
//! the listing replacement an ascent or descent makes, the pointer and
//! wheel paths, resize, and draw-stream and pixel checks at scales one
//! through four keeping every draw inside the finder's rectangle.

use td_ui::chrome::{ROW, SELECTED_ROW};
use td_ui::filter::insert;
use td_ui::finder::{
    matches, Choice, Choose, Controller, Entry, Error, Event, Key, Kind, Listing, Outcome, ENTRIES,
    LISTING_BYTES, META_BYTES, NAME_BYTES, NOTE_BYTES, PATH_BYTES, QUERY_BYTES,
};
use td_ui::raster::{
    Composition, Draw, Primitive, Raster, Rect, Scale, Surface, Weight, BORDER, CHROME, INK,
    LINE_NUMBER, PAPER,
};

fn surface(scale: u8) -> Surface {
    let s = scale as usize;
    Surface::new(640 * s, 400 * s, Scale::new(scale).unwrap()).unwrap()
}
fn rect(scale: u8) -> Rect {
    let s = i64::from(scale);
    Rect {
        x: 16 * s,
        y: 24 * s,
        width: 480 * scale as u32,
        height: 240 * scale as u32,
    }
}
fn entry(name: &str, kind: Kind) -> Entry {
    Entry::new(name, "", kind, true).unwrap()
}
/// Three folders, a disabled folder and two files, the usual test folder.
fn listing() -> Listing {
    Listing::new(
        "/home/tester/photos",
        vec![
            entry("2026-01-rome", Kind::Folder),
            entry("2026-02-oslo", Kind::Folder),
            entry("2026-03-lima", Kind::Folder),
            Entry::new("locked", "", Kind::Folder, false).unwrap(),
            Entry::new("notes.txt", "1 KiB", Kind::File, true).unwrap(),
            entry("DSC_0001.NEF", Kind::File),
        ],
        false,
    )
    .unwrap()
}
fn finder(choose: Choose, scale: u8) -> Controller {
    Controller::new(listing(), choose, surface(scale), rect(scale), None).unwrap()
}
fn key(f: &mut Controller, key: Key) -> Outcome {
    f.event(Event::Key {
        key,
        repeated: false,
    })
}
fn repeated(f: &mut Controller, key: Key) -> Outcome {
    f.event(Event::Key {
        key,
        repeated: true,
    })
}
fn typed(f: &mut Controller, text: &str) -> Vec<Outcome> {
    text.chars().map(|c| f.event(Event::Insert(c))).collect()
}
fn draws(f: &Controller, damage: Rect) -> Vec<Draw> {
    let mut draws = Vec::new();
    f.emit(damage, &mut |draw| draws.push(draw));
    draws
}
fn glyphs(draws: &[Draw]) -> Vec<(i64, i64, char, u32)> {
    draws
        .iter()
        .filter_map(|draw| match draw.primitive {
            Primitive::Glyph {
                x,
                y,
                scalar,
                style,
            } => Some((x, y, scalar, style.ink)),
            _ => None,
        })
        .collect()
}
fn text_at(draws: &[Draw], y: i64) -> String {
    let mut row: Vec<(i64, char)> = glyphs(draws)
        .into_iter()
        .filter(|(_, gy, _, _)| *gy == y)
        .map(|(x, _, c, _)| (x, c))
        .collect();
    row.sort();
    row.into_iter().map(|(_, c)| c).collect()
}

#[test]
fn a_listing_and_its_entries_are_bounded_and_control_free() {
    assert_eq!(
        Entry::new("", "", Kind::File, true).unwrap_err(),
        Error::InvalidText
    );
    assert_eq!(
        Entry::new("a\tb", "", Kind::File, true).unwrap_err(),
        Error::InvalidText
    );
    assert_eq!(
        Entry::new("a", "1\u{7f}", Kind::File, true).unwrap_err(),
        Error::InvalidText
    );
    assert_eq!(
        Entry::new(&"n".repeat(NAME_BYTES + 1), "", Kind::File, true).unwrap_err(),
        Error::Limit
    );
    assert!(Entry::new(
        &"n".repeat(NAME_BYTES),
        &"m".repeat(META_BYTES),
        Kind::File,
        true
    )
    .is_ok());
    assert_eq!(
        Entry::new("a", &"m".repeat(META_BYTES + 1), Kind::File, true).unwrap_err(),
        Error::Limit
    );
    assert_eq!(
        Listing::new("", Vec::new(), false).unwrap_err(),
        Error::InvalidText
    );
    assert_eq!(
        Listing::new("/a\nb", Vec::new(), false).unwrap_err(),
        Error::InvalidText
    );
    assert_eq!(
        Listing::new(&"/".repeat(PATH_BYTES + 1), Vec::new(), false).unwrap_err(),
        Error::Limit
    );
    let many: Vec<Entry> = (0..=ENTRIES).map(|_| entry("x", Kind::File)).collect();
    assert_eq!(Listing::new("/x", many, false).unwrap_err(), Error::Limit);
    // The name bytes between the entries are held under the listing bound
    // even when each entry is under its own.
    let big: Vec<Entry> = (0..(LISTING_BYTES / NAME_BYTES + 1))
        .map(|_| entry(&"n".repeat(NAME_BYTES), Kind::File))
        .collect();
    assert_eq!(Listing::new("/x", big, false).unwrap_err(), Error::Limit);
    let fits: Vec<Entry> = (0..(LISTING_BYTES / NAME_BYTES))
        .map(|_| entry(&"n".repeat(NAME_BYTES), Kind::File))
        .collect();
    let listing = Listing::new("/x", fits, true).unwrap();
    assert!(listing.truncated());
    assert_eq!(listing.entries().len(), LISTING_BYTES / NAME_BYTES);
    assert!(listing.storage_bytes() >= LISTING_BYTES);
    // The bound is the text's, not the capacity an allocator rounded to.
    let exact: Vec<Entry> = (0..ENTRIES)
        .map(|_| entry(&"n".repeat(LISTING_BYTES / ENTRIES), Kind::File))
        .collect();
    assert!(Listing::new("/x", exact, false).is_ok());
    let empty = Listing::new("/", Vec::new(), false).unwrap();
    assert_eq!(empty.path(), "/");
    assert!(empty.entries().is_empty());
}

#[test]
fn the_filter_rule_folds_ascii_refuses_the_rest_and_matches_every_term() {
    let mut query = String::new();
    assert!(insert(&mut query, 'R'));
    assert!(insert(&mut query, ' '));
    assert!(insert(&mut query, 'o'));
    assert!(!insert(&mut query, 'é'));
    assert!(!insert(&mut query, '\n'));
    assert_eq!(query, "r o");
    for _ in query.len()..QUERY_BYTES {
        assert!(insert(&mut query, 'x'));
    }
    assert!(!insert(&mut query, 'y'));
    assert_eq!(query.len(), QUERY_BYTES);
    assert!(matches("2026-01-Rome", "rome"));
    assert!(matches("2026-01-Rome", "ROME 2026"));
    assert!(matches("2026-01-Rome", ""));
    assert!(!matches("2026-01-Rome", "oslo"));
    assert!(!matches("2026-01-Rome", "rome oslo"));
    assert!(matches("Été", ""));
    assert!(!matches("Été", "ete"));
    assert_eq!(QUERY_BYTES, td_ui::filter::MAX_QUERY_BYTES);
    // The finder's rule is the launcher's over a folded name.
    for (name, query) in [
        ("2026-01-Rome", "rome"),
        ("2026-01-Rome", "ROME 2026"),
        ("2026-01-Rome", ""),
        ("2026-01-Rome", "oslo"),
        ("2026-01-Rome", "rome oslo"),
        ("DSC_0001.NEF", "dsc nef"),
        ("a b", "a  b"),
    ] {
        let mut folded = String::new();
        for c in query.chars() {
            insert(&mut folded, c);
        }
        assert_eq!(
            matches(name, &folded),
            td_ui::filter::matches(&name.to_ascii_lowercase(), &folded),
            "{name} / {query}"
        );
    }
}

#[test]
fn an_empty_listing_takes_every_key_and_press_and_answers_nothing() {
    let mut f = Controller::new(
        Listing::new("/empty", Vec::new(), true).unwrap(),
        Choose::File,
        surface(1),
        rect(1),
        Some("x"),
    )
    .unwrap();
    assert_eq!(f.selected(), None);
    for k in [
        Key::Up,
        Key::Down,
        Key::PageUp,
        Key::PageDown,
        Key::Home,
        Key::End,
        Key::Activate,
        Key::Accept,
    ] {
        assert_eq!(key(&mut f, k), Outcome::Consumed, "{k:?}");
    }
    let list = f.list_rect();
    assert_eq!(
        f.event(Event::Press {
            x: list.x + 8,
            y: list.y + 8
        }),
        Outcome::Consumed
    );
    assert_eq!(
        f.event(Event::Wheel {
            x: list.x + 8,
            y: list.y + 8,
            rows: 1
        }),
        Outcome::Consumed
    );
    // An empty read that stopped short says so; the body says empty.
    let all = draws(&f, surface(1).bounds());
    assert_eq!(text_at(&all, list.y + 4), "Empty folder");
    assert_eq!(text_at(&all, f.status_rect().y + 4), "0 entries, cut short");
    assert!(f.is_open());
}

#[test]
fn navigation_clamps_reveals_and_consumes_a_move_that_goes_nowhere() {
    let mut f = finder(Choose::Folder, 1);
    assert_eq!(f.shown(), &[0, 1, 2, 3, 4, 5]);
    assert_eq!(f.selected(), Some(0));
    assert_eq!(key(&mut f, Key::Up), Outcome::Consumed);
    assert_eq!(key(&mut f, Key::Down), Outcome::Changed);
    assert_eq!(f.selected(), Some(1));
    assert_eq!(key(&mut f, Key::End), Outcome::Changed);
    assert_eq!(f.selected(), Some(5));
    assert_eq!(key(&mut f, Key::Down), Outcome::Consumed);
    assert_eq!(key(&mut f, Key::PageUp), Outcome::Changed);
    assert_eq!(f.selected(), Some(0));
    assert_eq!(key(&mut f, Key::Home), Outcome::Consumed);
    assert_eq!(repeated(&mut f, Key::PageDown), Outcome::Changed);
    assert_eq!(f.selected(), Some(5));
    // A long listing scrolls: the window follows the selection the least.
    let long: Vec<Entry> = (0..40)
        .map(|i| entry(&format!("f{i:02}"), Kind::Folder))
        .collect();
    f.set_listing(Listing::new("/long", long, false).unwrap(), None)
        .unwrap();
    assert_eq!(f.first(), 0);
    let rows = f.list_rect().height as usize / ROW;
    assert_eq!(rows, 7);
    assert_eq!(key(&mut f, Key::End), Outcome::Changed);
    assert_eq!(f.first(), 40 - rows);
    assert_eq!(key(&mut f, Key::PageUp), Outcome::Changed);
    assert_eq!(f.selected(), Some(39 - rows));
    assert_eq!(f.first(), 39 - rows);
    assert_eq!(key(&mut f, Key::Home), Outcome::Changed);
    assert_eq!(f.first(), 0);
}

#[test]
fn the_filter_narrows_the_shown_resets_the_selection_and_backspace_edits_then_ascends() {
    let mut f = finder(Choose::Folder, 1);
    assert_eq!(key(&mut f, Key::End), Outcome::Changed);
    assert_eq!(typed(&mut f, "20"), [Outcome::Changed, Outcome::Changed]);
    assert_eq!(f.query(), "20");
    assert_eq!(f.shown(), &[0, 1, 2]);
    assert_eq!(f.selected(), Some(0));
    assert_eq!(typed(&mut f, "é"), [Outcome::Consumed]);
    assert_eq!(f.query(), "20");
    assert_eq!(typed(&mut f, " OSLO"), vec![Outcome::Changed; 5]);
    assert_eq!(f.query(), "20 oslo");
    assert_eq!(f.shown(), &[1]);
    assert_eq!(f.selected_entry().unwrap().name(), "2026-02-oslo");
    assert_eq!(typed(&mut f, "z"), [Outcome::Changed]);
    assert!(f.shown().is_empty());
    assert_eq!(f.selected(), None);
    assert_eq!(key(&mut f, Key::Activate), Outcome::Consumed);
    assert_eq!(key(&mut f, Key::Down), Outcome::Consumed);
    for _ in 0.."20 osloz".len() {
        assert_eq!(repeated(&mut f, Key::Backspace), Outcome::Changed);
    }
    assert_eq!(f.query(), "");
    assert_eq!(f.shown().len(), 6);
    // A repeated Backspace on an empty filter cannot run up the tree.
    assert_eq!(repeated(&mut f, Key::Backspace), Outcome::Consumed);
    assert_eq!(key(&mut f, Key::Backspace), Outcome::Ascend);
    assert_eq!(key(&mut f, Key::Parent), Outcome::Ascend);
    assert!(f.is_open());
}

#[test]
fn choosing_a_folder_descends_on_return_and_accepts_the_listed_folder() {
    let mut f = finder(Choose::Folder, 1);
    assert_eq!(key(&mut f, Key::Activate), Outcome::Descend(0));
    assert!(f.is_open());
    // Descending is the consumer's to carry out; the finder is unchanged.
    assert_eq!(f.listing().path(), "/home/tester/photos");
    assert_eq!(repeated(&mut f, Key::Activate), Outcome::Consumed);
    // A disabled folder and a file are not descended into.
    key(&mut f, Key::Down);
    key(&mut f, Key::Down);
    key(&mut f, Key::Down);
    assert_eq!(f.selected_entry().unwrap().name(), "locked");
    assert_eq!(key(&mut f, Key::Activate), Outcome::Consumed);
    key(&mut f, Key::Down);
    assert_eq!(f.selected_entry().unwrap().kind(), Kind::File);
    assert_eq!(key(&mut f, Key::Activate), Outcome::Consumed);
    // Accept is the listed folder, whatever the selection sits on.
    assert_eq!(key(&mut f, Key::Accept), Outcome::Closed(Choice::Here));
    assert!(!f.is_open());
    assert_eq!(key(&mut f, Key::Down), Outcome::Ignored);
    assert_eq!(f.event(Event::Insert('a')), Outcome::Ignored);
    assert!(draws(&f, surface(1).bounds()).is_empty());
    let mut f = finder(Choose::Folder, 1);
    assert_eq!(key(&mut f, Key::Escape), Outcome::Closed(Choice::Cancelled));
    let mut f = finder(Choose::Folder, 1);
    assert_eq!(repeated(&mut f, Key::Accept), Outcome::Consumed);
    assert_eq!(repeated(&mut f, Key::Escape), Outcome::Consumed);
    assert!(f.is_open());
}

#[test]
fn choosing_a_file_descends_folders_and_chooses_an_enabled_file_only() {
    let mut f = finder(Choose::File, 1);
    assert_eq!(key(&mut f, Key::Activate), Outcome::Descend(0));
    assert_eq!(key(&mut f, Key::Accept), Outcome::Consumed);
    key(&mut f, Key::End);
    assert_eq!(f.selected_entry().unwrap().name(), "DSC_0001.NEF");
    assert_eq!(key(&mut f, Key::Accept), Outcome::Closed(Choice::Entry(5)));
    let mut f = finder(Choose::File, 1);
    key(&mut f, Key::End);
    key(&mut f, Key::Up);
    assert_eq!(f.selected_entry().unwrap().name(), "notes.txt");
    assert_eq!(
        key(&mut f, Key::Activate),
        Outcome::Closed(Choice::Entry(4))
    );
    let mut f = Controller::new(
        Listing::new(
            "/x",
            vec![Entry::new("held", "", Kind::File, false).unwrap()],
            false,
        )
        .unwrap(),
        Choose::File,
        surface(1),
        rect(1),
        None,
    )
    .unwrap();
    assert_eq!(key(&mut f, Key::Activate), Outcome::Consumed);
    assert_eq!(key(&mut f, Key::Accept), Outcome::Consumed);
    assert!(f.is_open());
}

#[test]
fn a_new_listing_clears_the_filter_and_the_note_and_selects_the_named_entry() {
    let mut f = finder(Choose::Folder, 1);
    typed(&mut f, "oslo");
    f.set_note("cannot read locked: permission denied").unwrap();
    assert_eq!(f.note(), "cannot read locked: permission denied");
    assert_eq!(
        f.set_note(&"n".repeat(NOTE_BYTES + 1)).unwrap_err(),
        Error::Limit
    );
    assert_eq!(f.set_note("a\u{1}b").unwrap_err(), Error::InvalidText);
    let parent = Listing::new(
        "/home/tester",
        vec![
            entry("desktop", Kind::Folder),
            entry("photos", Kind::Folder),
            entry("videos", Kind::Folder),
        ],
        false,
    )
    .unwrap();
    f.set_listing(parent, Some("photos")).unwrap();
    // Construction selects by name the same way.
    let named = Controller::new(
        listing(),
        Choose::Folder,
        surface(1),
        rect(1),
        Some("locked"),
    )
    .unwrap();
    assert_eq!(named.selected(), Some(3));
    assert_eq!(f.query(), "");
    assert_eq!(f.note(), "");
    assert_eq!(f.listing().path(), "/home/tester");
    assert_eq!(f.selected_entry().unwrap().name(), "photos");
    // A name not listed lands on the first, as does none.
    f.set_listing(listing(), Some("gone")).unwrap();
    assert_eq!(f.selected(), Some(0));
    f.set_listing(listing(), None).unwrap();
    assert_eq!(f.selected(), Some(0));
    // The note survives until the next listing, through navigation.
    f.set_note("note").unwrap();
    key(&mut f, Key::Down);
    typed(&mut f, "x");
    assert_eq!(f.note(), "note");
    // A listing at the bound installs, and its indices are all shown.
    let full: Vec<Entry> = (0..ENTRIES)
        .map(|i| entry(&format!("{i}"), Kind::File))
        .collect();
    f.set_listing(Listing::new("/full", full, true).unwrap(), Some("4095"))
        .unwrap();
    assert_eq!(f.shown().len(), ENTRIES);
    assert_eq!(f.selected(), Some(ENTRIES - 1));
    assert!(f.storage_bytes() > ENTRIES * std::mem::size_of::<usize>());
}

#[test]
fn a_press_selects_a_shown_row_and_the_wheel_scrolls_the_window() {
    let mut f = finder(Choose::Folder, 1);
    let list = f.list_rect();
    let row = |i: i64| (list.x + 8, list.y + i * ROW as i64 + 8);
    let (x, y) = row(2);
    assert_eq!(f.event(Event::Press { x, y }), Outcome::Changed);
    assert_eq!(f.selected(), Some(2));
    assert_eq!(f.event(Event::Press { x, y }), Outcome::Consumed);
    assert_eq!(f.event(Event::Release { x, y }), Outcome::Consumed);
    assert_eq!(f.event(Event::Move { x, y }), Outcome::Consumed);
    // An empty row below the last entry, the filter and the path select
    // nothing; off the finder the pointer is the consumer's.
    let (x, y) = row(6);
    assert_eq!(f.event(Event::Press { x, y }), Outcome::Consumed);
    let outside = f.rect();
    for event in [
        Event::Press {
            x: outside.x - 1,
            y: outside.y,
        },
        Event::Release {
            x: outside.x,
            y: outside.y - 1,
        },
        Event::Move {
            x: outside.x + i64::from(outside.width),
            y: outside.y,
        },
        Event::Wheel {
            x: outside.x,
            y: outside.y + i64::from(outside.height),
            rows: 1,
        },
    ] {
        assert_eq!(f.event(event), Outcome::Ignored, "{event:?}");
    }
    assert_eq!(f.selected(), Some(2));
    let filter = f.filter_rect();
    assert_eq!(
        f.event(Event::Press {
            x: filter.x + 8,
            y: filter.y + 8
        }),
        Outcome::Consumed
    );
    let path = f.path_rect();
    assert_eq!(
        f.event(Event::Press {
            x: path.x + 8,
            y: path.y + 8
        }),
        Outcome::Consumed
    );
    assert_eq!(f.event(Event::Other), Outcome::Consumed);
    assert_eq!(f.selected(), Some(2));
    // The wheel over the list moves the window and keeps the selection
    // in it; over anything else it is consumed; a short list cannot move.
    let (x, y) = row(0);
    assert_eq!(f.event(Event::Wheel { x, y, rows: 1 }), Outcome::Consumed);
    assert_eq!(f.event(Event::Wheel { x, y, rows: 0 }), Outcome::Consumed);
    let long: Vec<Entry> = (0..40)
        .map(|i| entry(&format!("f{i:02}"), Kind::Folder))
        .collect();
    f.set_listing(Listing::new("/long", long, false).unwrap(), None)
        .unwrap();
    assert_eq!(f.event(Event::Wheel { x, y, rows: 3 }), Outcome::Changed);
    assert_eq!(f.first(), 3);
    assert_eq!(f.selected(), Some(3));
    assert_eq!(f.event(Event::Wheel { x, y, rows: -1 }), Outcome::Changed);
    assert_eq!(f.first(), 2);
    assert_eq!(f.selected(), Some(3));
    assert_eq!(f.event(Event::Wheel { x, y, rows: 100 }), Outcome::Changed);
    assert_eq!(f.first(), 33);
    assert_eq!(f.selected(), Some(33));
    assert_eq!(
        f.event(Event::Wheel {
            x: path.x,
            y: path.y,
            rows: -100
        }),
        Outcome::Consumed
    );
    assert_eq!(f.first(), 33);
    // A press on a row maps through the window.
    let (x, y) = row(1);
    assert_eq!(f.event(Event::Press { x, y }), Outcome::Changed);
    assert_eq!(f.selected(), Some(34));
}

#[test]
fn geometry_refuses_what_cannot_hold_the_finder_and_resize_relays_out() {
    let s = surface(1);
    let short = Rect {
        height: 3 * ROW as u32,
        ..rect(1)
    };
    assert_eq!(
        Controller::new(listing(), Choose::Folder, s, short, None).unwrap_err(),
        Error::NoRoom
    );
    let narrow = Rect {
        width: 19 * 8,
        ..rect(1)
    };
    assert_eq!(
        Controller::new(listing(), Choose::Folder, s, narrow, None).unwrap_err(),
        Error::NoRoom
    );
    let outside = Rect { x: 600, ..rect(1) };
    assert_eq!(
        Controller::new(listing(), Choose::Folder, s, outside, None).unwrap_err(),
        Error::NoRoom
    );
    let bad = Surface {
        width: 0,
        height: 0,
        scale: Scale::new(1).unwrap(),
    };
    assert_eq!(
        Controller::new(listing(), Choose::Folder, bad, rect(1), None).unwrap_err(),
        Error::InvalidSurface
    );
    // The smallest finder: four rows and twenty columns.
    let least = Rect {
        x: 0,
        y: 0,
        width: 20 * 8,
        height: 4 * ROW as u32,
    };
    let f = Controller::new(listing(), Choose::Folder, s, least, None).unwrap();
    assert_eq!(f.list_rect().height as usize, ROW);
    assert_eq!(f.status_rect().y, 3 * ROW as i64);
    // A remainder short of a row sits between the list and the status row.
    let ragged = Rect {
        height: 5 * ROW as u32 + 10,
        ..rect(1)
    };
    let f = Controller::new(listing(), Choose::Folder, s, ragged, None).unwrap();
    assert_eq!(f.list_rect().height as usize, 2 * ROW);
    assert_eq!(
        f.status_rect().y,
        ragged.y + i64::from(ragged.height) - ROW as i64
    );
    // Resize keeps the selection shown; a layout that cannot hold the
    // finder closes it unavailable.
    let mut f = finder(Choose::Folder, 1);
    key(&mut f, Key::End);
    let taller = Rect {
        height: 300,
        ..rect(1)
    };
    assert_eq!(
        f.event(Event::Resize {
            surface: s,
            rect: taller
        }),
        Outcome::Changed
    );
    assert_eq!(f.rect(), taller);
    assert_eq!(f.selected(), Some(5));
    assert_eq!(f.first(), 0);
    let two = surface(2);
    assert_eq!(
        f.event(Event::Resize {
            surface: two,
            rect: rect(2)
        }),
        Outcome::Changed
    );
    assert_eq!(f.list_rect().height as usize, 7 * ROW * 2);
    assert_eq!(
        f.event(Event::Resize {
            surface: s,
            rect: short
        }),
        Outcome::Closed(Choice::Unavailable(Error::NoRoom))
    );
    assert!(!f.is_open());
}

#[test]
fn the_draw_stream_stays_inside_the_rect_and_paints_each_band_at_every_scale() {
    for scale in 1..=4u8 {
        let mut f = finder(Choose::Folder, scale);
        let s = i64::from(scale);
        let surface = surface(scale);
        let rect = rect(scale);
        let all = draws(&f, surface.bounds());
        assert!(!all.is_empty());
        for draw in &all {
            assert_eq!(
                draw.clip.intersection(rect),
                Some(draw.clip),
                "outside the finder at scale {scale}: {draw:?}"
            );
        }
        // The path on the first row, in ink; the placeholder in the field;
        // the entries with the selection; a folder's meta; the status.
        assert_eq!(text_at(&all, rect.y + 4 * s), "/home/tester/photos");
        assert_eq!(text_at(&all, f.filter_rect().y + 4 * s), "Filter");
        let list = f.list_rect();
        // The rows carry the shared row painter's two-cell mark prefix.
        assert_eq!(text_at(&all, list.y + 4 * s), "  2026-01-romefolder");
        assert_eq!(
            text_at(&all, list.y + 4 * ROW as i64 * s + 4 * s),
            "  notes.txt1 KiB"
        );
        assert_eq!(text_at(&all, f.status_rect().y + 4 * s), "6 entries");
        // The selection's highlight is one list row wide.
        assert!(all.iter().any(|draw| matches!(
            draw.primitive,
            Primitive::Fill { rect, color }
                if color == SELECTED_ROW && rect.y == list.y && rect.height == (ROW as u32) * u32::from(scale)
        )));
        // The status rule under the list, one scaled pixel.
        let status = f.status_rect();
        assert!(all.iter().any(|draw| matches!(
            draw.primitive,
            Primitive::Fill { rect, color }
                if color == BORDER && rect.y == status.y && rect.height == u32::from(scale)
        )));
        // The disabled folder is dim; the selected row's label is ink.
        let dim = glyphs(&all)
            .iter()
            .filter(|(_, y, _, _)| *y == list.y + 3 * ROW as i64 * s + 4 * s)
            .map(|(_, _, _, ink)| *ink)
            .collect::<Vec<_>>();
        assert!(!dim.is_empty() && dim.iter().all(|ink| *ink != INK));
        // Partial damage clips every draw to it.
        let damage = Rect {
            x: list.x + 3 * s,
            y: list.y + 3 * s,
            width: 2,
            height: 2,
        };
        let clipped = draws(&f, damage);
        assert!(!clipped.is_empty());
        assert!(clipped.iter().all(|draw| draw.clip == damage));
        // Typing changes the field and the status; an empty match says so.
        typed(&mut f, "20");
        let filtered = draws(&f, surface.bounds());
        assert_eq!(text_at(&filtered, f.filter_rect().y + 4 * s), "20");
        assert_eq!(
            text_at(&filtered, f.status_rect().y + 4 * s),
            "3 of 6 match"
        );
        typed(&mut f, "q");
        let none = draws(&f, surface.bounds());
        assert_eq!(text_at(&none, list.y + 4 * s), "No match");
        assert_eq!(text_at(&none, f.status_rect().y + 4 * s), "0 of 6 match");
        f.set_note("cannot read locked: permission denied").unwrap();
        let noted = draws(&f, surface.bounds());
        assert_eq!(
            text_at(&noted, f.status_rect().y + 4 * s),
            "cannot read locked: permission denied"
        );
        // An empty folder, a cut-short one and a long path's tail.
        f.set_listing(
            Listing::new(&format!("/{}", "p".repeat(100)), Vec::new(), false).unwrap(),
            None,
        )
        .unwrap();
        let empty = draws(&f, surface.bounds());
        assert_eq!(text_at(&empty, list.y + 4 * s), "Empty folder");
        assert_eq!(text_at(&empty, f.status_rect().y + 4 * s), "0 entries");
        let columns = rect.width as usize / (8 * scale as usize) - 2;
        let path = text_at(&empty, rect.y + 4 * s);
        assert_eq!(path.chars().count(), columns);
        assert!(path.starts_with('\u{2026}') && path.ends_with('p'));
        f.set_listing(
            Listing::new("/one", vec![entry("only", Kind::File)], true).unwrap(),
            None,
        )
        .unwrap();
        let cut = draws(&f, surface.bounds());
        assert_eq!(
            text_at(&cut, f.status_rect().y + 4 * s),
            "1 entry, cut short"
        );
        typed(&mut f, "o");
        let cut = draws(&f, surface.bounds());
        assert_eq!(
            text_at(&cut, f.status_rect().y + 4 * s),
            "1 of 1 match, cut short"
        );
        // Counts of more than one digit stream whole.
        let forty: Vec<Entry> = (0..40)
            .map(|i| entry(&format!("f{i:02}"), Kind::Folder))
            .collect();
        f.set_listing(Listing::new("/forty", forty, false).unwrap(), None)
            .unwrap();
        let forty = draws(&f, surface.bounds());
        assert_eq!(text_at(&forty, f.status_rect().y + 4 * s), "40 entries");
        typed(&mut f, "f1");
        let forty = draws(&f, surface.bounds());
        assert_eq!(text_at(&forty, f.status_rect().y + 4 * s), "10 of 40 match");
        let full: Vec<Entry> = (0..ENTRIES)
            .map(|i| entry(&format!("{i}"), Kind::File))
            .collect();
        f.set_listing(Listing::new("/full", full, false).unwrap(), None)
            .unwrap();
        let full = draws(&f, surface.bounds());
        assert_eq!(text_at(&full, f.status_rect().y + 4 * s), "4096 entries");
    }
}

#[test]
fn a_meta_the_row_cannot_hold_beside_the_label_is_left_out() {
    let s = surface(1);
    let wide = Entry::new("name", &"m".repeat(META_BYTES), Kind::File, true).unwrap();
    let narrow = Entry::new("name", "1 KiB", Kind::File, true).unwrap();
    let listing = || Listing::new("/x", vec![], false).unwrap();
    let mut f = Controller::new(listing(), Choose::File, s, rect(1), None).unwrap();
    f.set_listing(Listing::new("/x", vec![wide, narrow], false).unwrap(), None)
        .unwrap();
    // The usual width holds the widest meta beside eight label cells.
    let all = draws(&f, s.bounds());
    let list = f.list_rect();
    assert_eq!(
        text_at(&all, list.y + 4),
        format!("  name{}", "m".repeat(META_BYTES))
    );
    assert_eq!(text_at(&all, list.y + 24 + 4), "  name1 KiB");
    // The least finder cannot: the label shows, the wide meta does not,
    // and the short one still does.
    let least = Rect {
        x: 0,
        y: 0,
        width: 20 * 8,
        height: 4 * 24,
    };
    let wide = Entry::new("name", &"m".repeat(META_BYTES), Kind::File, true).unwrap();
    let narrow = Entry::new("name", "1 KiB", Kind::File, true).unwrap();
    let mut f = Controller::new(listing(), Choose::File, s, least, None).unwrap();
    f.set_listing(Listing::new("/x", vec![wide, narrow], false).unwrap(), None)
        .unwrap();
    let list = f.list_rect();
    let all = draws(&f, s.bounds());
    assert_eq!(text_at(&all, list.y + 4), "  name");
    key(&mut f, Key::Down);
    let all = draws(&f, s.bounds());
    assert_eq!(text_at(&all, list.y + 4), "  name1 KiB");
}

#[test]
fn a_long_query_keeps_its_caret_shown_in_a_narrow_field() {
    let s = surface(1);
    let least = Rect {
        x: 0,
        y: 0,
        width: 20 * 8,
        height: 4 * 24,
    };
    let mut f = Controller::new(listing(), Choose::Folder, s, least, None).unwrap();
    let outcomes = typed(&mut f, &"q".repeat(QUERY_BYTES));
    assert!(outcomes.iter().all(|o| *o == Outcome::Changed));
    assert_eq!(f.query().len(), QUERY_BYTES);
    let all = draws(&f, s.bounds());
    let field = f.filter_rect();
    // The field shows its last columns and the caret sits at their end,
    // inside the field.
    let shown = text_at(&all, field.y + 4);
    assert!(shown.len() < QUERY_BYTES && shown.chars().all(|c| c == 'q'));
    let caret = all.iter().find_map(|draw| match draw.primitive {
        Primitive::Fill { rect, color }
            if color == INK && rect.width == 1 && field.contains(rect.x, rect.y) =>
        {
            Some(rect)
        }
        _ => None,
    });
    let caret = caret.expect("the caret is painted inside the field");
    assert_eq!(caret.x, field.x + 8 + shown.len() as i64 * 8);
}

#[test]
fn the_finder_rasterizes_to_pixels_and_leaves_the_rest_of_the_surface_alone() {
    struct Scene {
        surface: Surface,
        draws: Vec<Draw>,
    }
    impl Composition for Scene {
        fn surface(&self) -> Surface {
            self.surface
        }
        fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
            for draw in &self.draws {
                if let Some(clip) = draw.clip.intersection(damage) {
                    sink(Draw { clip, ..*draw });
                }
            }
        }
    }
    let face = td_ui::font::pinned().unwrap();
    for scale in 1..=4u8 {
        let f = finder(Choose::Folder, scale);
        let surface = surface(scale);
        let rect = rect(scale);
        let s = scale as usize;
        let stream = draws(&f, surface.bounds());
        let mut bytes = vec![0x7a; surface.width * surface.height * 4];
        Raster::new(&mut bytes, &face, surface, surface.width * 4)
            .unwrap()
            .paint(
                &Scene {
                    surface,
                    draws: stream,
                },
                surface.bounds(),
            )
            .unwrap();
        let color = |x: i64, y: i64| {
            let start = (y as usize * surface.width + x as usize) * 4;
            u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap()) & 0xffffff
        };
        // The path row is chrome, the field paper, the selection its row
        // colour, the second row chrome, the status row chrome under its
        // border, and the surface outside untouched.
        assert_eq!(color(rect.x + 2, rect.y + 2), CHROME);
        let filter = f.filter_rect();
        assert_eq!(color(filter.x + 2, filter.y + 2), PAPER);
        let list = f.list_rect();
        assert_eq!(
            color(list.x + 200 * s as i64, list.y + 2),
            SELECTED_ROW & 0xffffff
        );
        assert_eq!(
            color(list.x + 200 * s as i64, list.y + (ROW * s) as i64 + 2),
            CHROME
        );
        let status = f.status_rect();
        assert_eq!(color(status.x + 2, status.y), BORDER);
        assert_eq!(color(status.x + 2, status.y + s as i64 + 1), CHROME);
        let has_status_ink = (status.y..status.y + i64::from(status.height))
            .any(|y| (status.x..status.x + 100).any(|x| color(x, y) == LINE_NUMBER));
        assert!(has_status_ink, "the status row paints its line");
        let has_label_ink = (list.y..list.y + (ROW * s) as i64)
            .any(|y| (list.x..list.x + 200 * s as i64).any(|x| color(x, y) == INK));
        assert!(has_label_ink, "the selection paints its label");
        assert_eq!(color(0, 0), 0x7a7a7a);
        assert_eq!(color(rect.x - 1, rect.y + 10), 0x7a7a7a);
        assert_eq!(color(rect.x + i64::from(rect.width), rect.y + 10), 0x7a7a7a);
        assert_eq!(
            color(rect.x + 10, rect.y + i64::from(rect.height)),
            0x7a7a7a
        );
        // The glyphs are medium weight throughout.
        assert!(glyphs(&draws(&f, surface.bounds())).len() > 20);
        assert!(draws(&f, surface.bounds())
            .iter()
            .all(|draw| match draw.primitive {
                Primitive::Glyph { style, .. } => style.weight == Weight::Medium,
                _ => true,
            }));
    }
}
