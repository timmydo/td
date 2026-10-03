//! The window's state driven as a person would, its jobs run on the test's
//! thread against a real directory.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::fs;
use std::path::PathBuf;

use td_dua::app::App;
use td_dua::scan::Progress;
use td_dua::tree::{NodeId, ROOT};
use td_dua::worker;
use td_ui::raster::{Scale, Surface};
use td_ui::window::{Input, PointerPhase};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("td-dua-app-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// big.bin 40 KiB, docs/{a.txt 8 KiB, b.txt 4 KiB}, small.txt 1 byte.
fn sample(name: &str) -> Scratch {
    let scratch = Scratch::new(name);
    let root = &scratch.0;
    fs::write(root.join("big.bin"), vec![7u8; 40 * 1024]).unwrap();
    fs::create_dir(root.join("docs")).unwrap();
    fs::write(root.join("docs/a.txt"), vec![1u8; 8 * 1024]).unwrap();
    fs::write(root.join("docs/b.txt"), vec![1u8; 4 * 1024]).unwrap();
    fs::write(root.join("small.txt"), b"x").unwrap();
    scratch
}

fn surface() -> Surface {
    Surface::new(1024, 768, Scale::default()).unwrap()
}

/// Runs every job the app has asked for, and those its replies ask for.
fn settle(app: &mut App) {
    let progress = Progress::default();
    loop {
        let jobs = app.take_jobs();
        if jobs.is_empty() {
            break;
        }
        for job in jobs {
            app.reply(worker::run(job, &progress));
        }
    }
    assert!(!app.busy());
}

fn open(scratch: &Scratch) -> App {
    let mut app = App::new(scratch.0.clone(), surface());
    settle(&mut app);
    app
}

fn key(app: &mut App, chord: &str) {
    app.input(Input::Key {
        chord,
        repeat: false,
    });
}

fn node(app: &App, path: &str) -> NodeId {
    let tree = app.tree().unwrap();
    let mut at = ROOT;
    for part in path.split('/') {
        at = *tree
            .get(at)
            .unwrap()
            .children
            .iter()
            .find(|c| tree.get(**c).unwrap().name == part)
            .unwrap_or_else(|| panic!("no {path}"));
    }
    at
}

fn select(app: &mut App, path: &str) {
    let id = node(app, path);
    app.reveal(id);
    assert_eq!(app.selected(), Some(id));
}

fn rows(app: &App) -> Vec<String> {
    let tree = app.tree().unwrap();
    app.table()
        .unwrap()
        .model()
        .rows()
        .iter()
        .map(|row| match row.id {
            td_dua::view::RowId::Node(id) => {
                tree.get(id).unwrap().name.to_string_lossy().into_owned()
            }
            td_dua::view::RowId::More(_) => "…".to_owned(),
        })
        .collect()
}

#[test]
fn the_list_starts_sorted_by_size_with_the_root_open() {
    let scratch = sample("sorted");
    let mut app = open(&scratch);
    let root = scratch.0.to_string_lossy().into_owned();
    assert_eq!(rows(&app), [&root, "big.bin", "docs", "small.txt"]);
    assert_eq!(app.selected(), Some(ROOT));
    // Down selects the largest entry; Right on docs opens it.
    key(&mut app, "Down");
    assert_eq!(app.selected(), Some(node(&app, "big.bin")));
    key(&mut app, "Down");
    key(&mut app, "Right");
    assert_eq!(
        rows(&app),
        [&root, "big.bin", "docs", "a.txt", "b.txt", "small.txt"]
    );
    // Clicking the Name heading sorts A to Z, again Z to A.
    let table = app.table().unwrap();
    let header = table.geometry().unwrap().header_cell(0).unwrap().rect;
    let (x, y) = (header.x + 4, header.y + 4);
    for _ in 0..2 {
        for phase in [PointerPhase::Press, PointerPhase::Release] {
            app.input(Input::Pointer {
                phase,
                x,
                y,
                extend: false,
                follow: false,
            });
        }
    }
    assert_eq!(
        rows(&app),
        [&root, "small.txt", "docs", "b.txt", "a.txt", "big.bin"]
    );
}

#[test]
fn a_treemap_click_selects_the_file_and_opens_the_list_to_it() {
    let scratch = sample("click");
    let mut app = open(&scratch);
    app.prepare();
    let a = node(&app, "docs/a.txt");
    let docs = node(&app, "docs");
    assert!(!app.expanded(docs));
    let tile = app
        .treemap()
        .tiles
        .iter()
        .find(|tile| tile.node == a)
        .copied()
        .unwrap();
    let area = app.treemap_rect().unwrap();
    assert!(area.intersection(tile.rect) == Some(tile.rect));
    let (x, y) = (
        tile.rect.x + i64::from(tile.rect.width) / 2,
        tile.rect.y + i64::from(tile.rect.height) / 2,
    );
    app.input(Input::Pointer {
        phase: PointerPhase::Press,
        x,
        y,
        extend: false,
        follow: false,
    });
    app.input(Input::Pointer {
        phase: PointerPhase::Release,
        x,
        y,
        extend: false,
        follow: false,
    });
    assert_eq!(app.selected(), Some(a));
    assert!(app.expanded(docs));
    assert!(rows(&app).contains(&"a.txt".to_owned()));
}

#[test]
fn the_delete_list_adds_undoes_and_deletes_after_confirming() {
    let scratch = sample("list");
    let mut app = open(&scratch);
    // The root itself is never queued.
    key(&mut app, "d");
    assert!(app.queue().is_empty());
    select(&mut app, "small.txt");
    key(&mut app, "d");
    select(&mut app, "docs");
    key(&mut app, "d");
    // A child of a queued directory is already covered.
    select(&mut app, "docs/a.txt");
    key(&mut app, "d");
    assert_eq!(app.queue().len(), 2);
    key(&mut app, "u");
    assert_eq!(app.queue(), &[node(&app, "small.txt")]);
    select(&mut app, "docs");
    key(&mut app, "d");
    assert_eq!(app.queue().len(), 2);
    // x asks; Escape keeps everything.
    key(&mut app, "x");
    assert!(app.dialog_open());
    key(&mut app, "Escape");
    assert!(!app.dialog_open());
    assert!(app.take_jobs().is_empty());
    // Enter on the initially focused Cancel keeps everything too.
    key(&mut app, "x");
    key(&mut app, "Return");
    assert!(!app.dialog_open());
    assert!(app.take_jobs().is_empty());
    assert!(scratch.0.join("docs").exists());
    // Tab to Delete, then Enter.
    key(&mut app, "x");
    key(&mut app, "Tab");
    key(&mut app, "Return");
    assert!(!app.dialog_open());
    settle(&mut app);
    assert!(!scratch.0.join("docs").exists());
    assert!(!scratch.0.join("small.txt").exists());
    assert!(scratch.0.join("big.bin").exists());
    assert!(app.queue().is_empty());
    let root = app.tree().unwrap().root().unwrap();
    assert_eq!(root.files, 1);
    assert!(app.message().starts_with("Deleted 2"), "{}", app.message());
}

#[test]
fn the_key_list_is_the_window_s_keys_then_the_list_s_and_the_question_s() {
    let scratch = sample("keys");
    let mut app = open(&scratch);
    let titles = |app: &App| -> Vec<&str> { app.key_list().iter().map(|s| s.title).collect() };
    assert_eq!(titles(&app), ["Disk usage", "List", "Delete question"]);
    let sections = app.key_list();
    let window: Vec<(&str, &str)> = sections[0].rows.iter().map(|r| (r.keys, r.what)).collect();
    assert_eq!(window, td_dua::app::KEYS);
    assert!(sections[1].rows.iter().any(|r| r.keys == "S-Left/S-Right"));
    // A "more" row shows `view::SHOWN` more, as the list says.
    let more = format!("show {} more", td_dua::view::SHOWN);
    assert!(sections[1]
        .rows
        .iter()
        .any(|r| r.keys == "Return/Space" && r.what.ends_with(&more)));
    // The status row's hint is the same table, its lead first and the
    // window's keys after the scan's message.
    let line = app.status_line();
    let (lead, keys) = td_dua::app::hint()
        .split_once("  ")
        .map(|(lead, keys)| (lead.to_owned(), keys.to_owned()))
        .unwrap();
    assert!(line.starts_with(&format!("{lead}  ")), "{line}");
    assert!(line.ends_with(&format!(".  {keys}")), "{line}");
    assert_eq!(
        td_dua::app::hint(),
        "F1: keys  d: add to delete list  D: delete now  x: delete the list  u: undo add  r: refresh  a: allocated or apparent size  C-q: quit"
    );
    // While the question is open its keys come first.
    select(&mut app, "small.txt");
    key(&mut app, "d");
    key(&mut app, "x");
    assert!(app.dialog_open());
    assert_eq!(titles(&app), ["Delete question", "Disk usage", "List"]);
}

/// Both orders of the key list, the question closed and open, are spelled
/// as td-ui's keymap spells chords, titled with a capital and described.
#[test]
fn the_key_list_passes_td_ui_s_check() {
    let scratch = sample("check");
    let mut app = open(&scratch);
    let problems = td_ui::keys::check(&app.key_list());
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    select(&mut app, "small.txt");
    key(&mut app, "d");
    key(&mut app, "x");
    assert!(app.dialog_open());
    let problems = td_ui::keys::check(&app.key_list());
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

fn press(app: &mut App, phase: PointerPhase, x: i64, y: i64) {
    app.input(Input::Pointer {
        phase,
        x,
        y,
        extend: false,
        follow: false,
    });
}

/// A press on the status row, whose hint starts with `F1: keys`, asks the
/// window for its key list, once a press, and changes nothing else; a
/// press anywhere else, or on the status row while the delete question
/// takes the pointer, asks for nothing.
#[test]
fn a_press_on_the_status_row_opens_the_key_list() {
    let scratch = sample("status");
    let mut app = open(&scratch);
    let status = td_ui::chrome::Status::new(surface()).rect();
    let (x, y) = (status.x + 40, status.y + status.height as i64 / 2);
    assert!(
        app.status_line().starts_with("F1: keys  "),
        "{}",
        app.status_line()
    );
    assert!(!app.take_show_keys());
    let selected = app.selected();
    press(&mut app, PointerPhase::Press, x, y);
    press(&mut app, PointerPhase::Release, x, y);
    assert!(app.take_show_keys());
    assert!(!app.take_show_keys(), "an edge, taken once");
    assert_eq!(app.selected(), selected);
    assert!(app.take_jobs().is_empty());
    // Just above the status row is the treemap, not the list.
    press(&mut app, PointerPhase::Press, x, status.y - 1);
    press(&mut app, PointerPhase::Release, x, status.y - 1);
    assert!(!app.take_show_keys());
    // A key reaches nobody's list: the window's own F1 does that.
    key(&mut app, "F1");
    assert!(!app.take_show_keys());
    select(&mut app, "small.txt");
    key(&mut app, "d");
    key(&mut app, "x");
    assert!(app.dialog_open());
    press(&mut app, PointerPhase::Press, x, y);
    press(&mut app, PointerPhase::Release, x, y);
    assert!(!app.take_show_keys(), "the question takes the pointer");
}

/// The status row opens the key list only while it shows the hint's
/// lead whole: a message without the hint, a row too narrow for the
/// lead before its ellipsis, or a row the surface cuts, opens nothing.
#[test]
fn a_status_row_without_its_hint_whole_opens_nothing() {
    let scratch = sample("status-unhinted");
    let mut app = open(&scratch);
    let at = |surface: Surface| {
        let status = td_ui::chrome::Status::new(surface).rect();
        (status.x + 20, status.y + status.height as i64 / 2)
    };
    let (x, y) = at(surface());
    key(&mut app, "x");
    assert_eq!(
        app.message(),
        "The delete list is empty: add entries with d"
    );
    assert!(!app.status_line().contains("F1"), "{}", app.status_line());
    press(&mut app, PointerPhase::Press, x, y);
    press(&mut app, PointerPhase::Release, x, y);
    assert!(!app.take_show_keys(), "a message without the hint");
    // A refresh's message carries the hint again.
    key(&mut app, "r");
    settle(&mut app);
    assert!(app.status_line().starts_with("F1: keys  "));
    press(&mut app, PointerPhase::Press, x, y);
    assert!(app.take_show_keys());
    // Eight cells hold the lead only when the line is no longer; nine hold
    // it before the ellipsis.
    for (width, height, opens) in [(80, 600, false), (88, 600, true), (800, 20, false)] {
        let narrow = Surface::new(width, height, Scale::default()).unwrap();
        app.input(Input::Resize(narrow));
        let (x, y) = at(narrow);
        press(&mut app, PointerPhase::Press, x, y.max(0));
        press(&mut app, PointerPhase::Release, x, y.max(0));
        assert_eq!(app.take_show_keys(), opens, "{width}x{height}");
    }
}

#[test]
fn a_changed_list_closes_the_question_unanswered() {
    let scratch = sample("stale");
    let mut app = open(&scratch);
    select(&mut app, "small.txt");
    key(&mut app, "d");
    key(&mut app, "x");
    assert!(app.dialog_open());
    // Any change to the tree or the list bumps the revision; the toggle
    // of the measure is one that a key behind the dialog cannot make, so
    // the test refreshes the state through a reply instead.
    let progress = Progress::default();
    app.reply(worker::run(
        worker::Job::Scan {
            path: scratch.0.clone(),
            at: Some(ROOT),
        },
        &progress,
    ));
    key(&mut app, "Tab");
    assert!(!app.dialog_open());
    assert!(app.take_jobs().is_empty());
    assert!(scratch.0.join("small.txt").exists());
}

#[test]
fn shift_d_deletes_the_selection_at_once() {
    let scratch = sample("now");
    let mut app = open(&scratch);
    select(&mut app, "docs/b.txt");
    key(&mut app, "D");
    assert!(app.busy());
    // Nothing else deletes or refreshes while that runs.
    key(&mut app, "r");
    let jobs = app.take_jobs();
    assert_eq!(jobs.len(), 1);
    let id = node(&app, "docs/b.txt");
    let path = scratch.0.join("docs/b.txt");
    let progress = Progress::default();
    for job in jobs {
        app.reply(worker::run(job, &progress));
    }
    assert!(!path.exists());
    assert!(app.tree().unwrap().get(id).is_none());
    assert_eq!(
        app.tree().unwrap().get(node(&app, "docs")).unwrap().files,
        1
    );
    // Nothing is selected after: a second D (or Caps Lock's d) deletes
    // nothing more, least of all the directory that held the entry.
    assert_eq!(app.selected(), None);
    key(&mut app, "D");
    assert!(app.take_jobs().is_empty());
    assert_eq!(app.message(), "Select an entry first");
    assert!(scratch.0.join("docs/a.txt").exists());
}

#[test]
fn r_refreshes_the_selected_directory() {
    let scratch = sample("refresh");
    let mut app = open(&scratch);
    fs::write(scratch.0.join("docs/new.txt"), vec![0u8; 100_000]).unwrap();
    select(&mut app, "docs/a.txt");
    let before = app.tree().unwrap().root().unwrap().files;
    // A file refreshes the directory holding it.
    key(&mut app, "r");
    settle(&mut app);
    let docs = node(&app, "docs");
    let tree = app.tree().unwrap();
    assert_eq!(tree.get(docs).unwrap().files, 3);
    assert_eq!(tree.root().unwrap().files, before + 1);
    let new = node(&app, "docs/new.txt");
    let tree = app.tree().unwrap();
    assert_eq!(tree.get(new).unwrap().total.apparent, 100_000);
    assert!(tree.root().unwrap().total.apparent >= 100_000 + 52 * 1024);
    // The old selection's node was replaced; its new node, found by path,
    // takes the selection.
    assert_eq!(app.selected(), Some(node(&app, "docs/a.txt")));
}

#[test]
fn the_frame_paints_at_every_scale() {
    let scratch = sample("paint");
    for scale in 1..=4u8 {
        let surface = Surface::new(1200, 900, Scale::new(scale).unwrap()).unwrap();
        let mut app = App::new(scratch.0.clone(), surface);
        settle(&mut app);
        select(&mut app, "docs/a.txt");
        app.prepare();
        let frame = td_ui::driven::paint(&app.frame()).unwrap();
        assert!(!frame.ppm().is_empty());
        let (_, _, text) = td_ui::driven::text(&app.frame()).unwrap();
        assert!(text.contains("a.txt"), "{text}");
    }
}

#[test]
fn an_entry_the_list_cannot_show_leaves_nothing_selected() {
    let scratch = Scratch::new("deep");
    let mut deep = scratch.0.clone();
    for _ in 0..td_ui::tree_table::DEPTH + 4 {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("f"), b"x").unwrap();
    fs::write(scratch.0.join("top"), b"y").unwrap();
    let mut app = open(&scratch);
    select(&mut app, "top");
    let mut path = vec!["d"; td_ui::tree_table::DEPTH + 4];
    path.push("f");
    let id = node(&app, &path.join("/"));
    app.reveal(id);
    assert_eq!(app.selected(), None);
    assert!(
        app.message().contains("cannot be shown"),
        "{}",
        app.message()
    );
    key(&mut app, "D");
    assert!(app.take_jobs().is_empty());
    assert!(scratch.0.join("top").exists());
}

#[test]
fn the_more_row_is_no_delete_target_and_shows_more() {
    let scratch = Scratch::new("more");
    for index in 0..td_dua::view::SHOWN + 2 {
        fs::write(scratch.0.join(format!("f{index:04}")), b"x").unwrap();
    }
    let mut app = open(&scratch);
    assert_eq!(rows(&app).len(), td_dua::view::SHOWN + 2);
    key(&mut app, "End");
    assert_eq!(rows(&app).last().map(String::as_str), Some("…"));
    assert_eq!(app.selected(), None);
    key(&mut app, "d");
    key(&mut app, "D");
    assert!(app.queue().is_empty());
    assert!(app.take_jobs().is_empty());
    key(&mut app, "Return");
    assert_eq!(rows(&app).len(), td_dua::view::SHOWN + 3);
    assert!(app.selected().is_some());
}

#[test]
fn a_surviving_hard_link_takes_the_bytes_of_a_deleted_one() {
    let scratch = Scratch::new("links");
    fs::create_dir(scratch.0.join("a")).unwrap();
    fs::create_dir(scratch.0.join("b")).unwrap();
    fs::write(scratch.0.join("a/one"), vec![1u8; 10_000]).unwrap();
    fs::hard_link(scratch.0.join("a/one"), scratch.0.join("b/two")).unwrap();
    let mut app = open(&scratch);
    let tree = app.tree().unwrap();
    let (one, two) = (node(&app, "a/one"), node(&app, "b/two"));
    let (owner, other, owner_path) = if tree.get(one).unwrap().own.apparent > 0 {
        (one, "b/two", "a/one")
    } else {
        (two, "a/one", "b/two")
    };
    app.reveal(owner);
    key(&mut app, "D");
    settle(&mut app);
    assert!(!scratch.0.join(owner_path).exists());
    let survivor = node(&app, other);
    let tree = app.tree().unwrap();
    assert_eq!(tree.get(survivor).unwrap().own.apparent, 10_000);
    assert!(tree.root().unwrap().total.apparent >= 10_000);
    assert!(app.message().contains("freeing 0 B"), "{}", app.message());
}

#[test]
fn a_root_refresh_replaces_the_tree_and_keeps_state_by_path() {
    let scratch = sample("rootrefresh");
    let mut app = open(&scratch);
    select(&mut app, "small.txt");
    key(&mut app, "d");
    select(&mut app, "big.bin");
    key(&mut app, "d");
    select(&mut app, "docs/a.txt");
    fs::remove_file(scratch.0.join("big.bin")).unwrap();
    let slots = app.tree().unwrap().len();
    key(&mut app, "Home");
    assert_eq!(app.selected(), Some(ROOT));
    key(&mut app, "r");
    settle(&mut app);
    // A fresh tree, not a grown one.
    assert!(app.tree().unwrap().len() < slots);
    assert_eq!(app.queue(), &[node(&app, "small.txt")]);
    assert!(app.expanded(node(&app, "docs")));
    assert_eq!(app.selected(), Some(ROOT));
    assert!(
        app.message().contains("1 entry left the delete list"),
        "{}",
        app.message()
    );
}

#[test]
fn a_refresh_keeps_a_selection_made_elsewhere_meanwhile() {
    let scratch = sample("elsewhere");
    let mut app = open(&scratch);
    select(&mut app, "docs");
    key(&mut app, "r");
    // While the scan runs, the person moves to an entry outside docs.
    select(&mut app, "small.txt");
    settle(&mut app);
    assert_eq!(app.selected(), Some(node(&app, "small.txt")));
}

#[test]
fn a_refreshed_selection_that_vanished_leaves_nothing_selected() {
    let scratch = sample("vanished");
    let mut app = open(&scratch);
    select(&mut app, "docs/a.txt");
    fs::remove_file(scratch.0.join("docs/a.txt")).unwrap();
    key(&mut app, "r");
    settle(&mut app);
    // Not docs: a delete key must not fall to the directory.
    assert_eq!(app.selected(), None);
    key(&mut app, "D");
    assert!(app.take_jobs().is_empty());
    assert!(scratch.0.join("docs").exists());
}

#[test]
fn a_refreshed_more_row_stays_a_more_row() {
    let scratch = Scratch::new("more-refresh");
    fs::create_dir(scratch.0.join("x")).unwrap();
    for index in 0..td_dua::view::SHOWN + 2 {
        fs::write(scratch.0.join(format!("x/f{index:04}")), b"x").unwrap();
    }
    let mut app = open(&scratch);
    select(&mut app, "x");
    let x = node(&app, "x");
    key(&mut app, "End");
    assert_eq!(
        app.table().unwrap().selected(),
        Some(td_dua::view::RowId::More(x))
    );
    key(&mut app, "r");
    settle(&mut app);
    assert_eq!(
        app.table().unwrap().selected(),
        Some(td_dua::view::RowId::More(x))
    );
    assert_eq!(app.selected(), None);
    key(&mut app, "D");
    assert!(app.take_jobs().is_empty());
    assert!(scratch.0.join("x").exists());
}
