#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The typeface over fonts `fonts` encodes: a face fitted to the grid's
//! cell at each scale, made once, and the refusal of a face no grid cell
//! fits; a raster given a typeface drawing through the face fitted at its
//! scale; the pinned face's loaders, the regular style and a terminal's
//! four, over a directory the test writes; and the search for that
//! directory over trees the test writes.

mod fonts;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use fonts::{Builder, Glyph, Segment};
use td_ui::atlas::{Slot, Style};
use td_ui::face::{Face, Sizing};
use td_ui::face_file::{
    find, places, Place, BOLD, BOLD_ITALIC, DIR, INSTALLED, INSTALL_HINT, ITALIC, REGULAR,
    SEARCH_DEPTH, SEARCH_ENTRIES,
};
use td_ui::font::pinned;
use td_ui::notices::OUTLINE_FACE;
use td_ui::pinned_face::{load_from, load_in, load_in_or_note, styles_in_or_note, SETTING};
use td_ui::raster::{Draw, GlyphStyle, Primitive, Raster, Rect, Scale, Surface, Weight};
use td_ui::sfnt::{Error, MAX_FONT_BYTES};
use td_ui::typeface::Typeface;
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

/// The bitmap grid's cell, as a terminal fits its face to it.
const CELL: Sizing = Sizing::Cell {
    width: CELL_WIDTH,
    height: CELL_HEIGHT,
};

/// '0' a 500-unit square on the baseline.
fn font() -> Vec<u8> {
    let square = vec![
        (0, 0, true),
        (0, 500, true),
        (500, 500, true),
        (500, 0, true),
    ];
    let mut builder = Builder::new(vec![Glyph::Empty, Glyph::Simple(vec![square])]);
    builder.format4 = vec![Segment::Delta(0x30, 0x30, 1u16.wrapping_sub(0x30))];
    builder.font()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("td-ui-typeface-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_face_is_fitted_to_the_grid_at_each_scale_and_kept_while_it_holds() {
    let mut typeface = Typeface::new(font(), Some(font())).unwrap();
    for scale in 1..=4 {
        let face = typeface.face(Scale::new(scale).unwrap()).unwrap();
        let cell = face.cell();
        let size = usize::from(scale);
        assert_eq!(
            (cell.width, cell.height),
            (CELL_WIDTH * size, CELL_HEIGHT * size)
        );
        assert!(matches!(face.glyph(Style::Regular, '0'), Slot::Placed(_)));
        // Asked again at the same scale, the face is the one made, atlas
        // and all.
        let again = typeface.face(Scale::new(scale).unwrap()).unwrap();
        assert_eq!(again.atlas().len(), 1, "scale {scale}");
    }
    // Another scale's face replaces it: one atlas is held.
    let face = typeface.face(Scale::new(1).unwrap()).unwrap();
    assert_eq!(face.atlas().len(), 0);
}

#[test]
fn a_raster_given_a_typeface_draws_through_the_face_fitted_at_its_scale() {
    let bitmap = pinned().unwrap();
    let draws = |x: i64| {
        ['0', 'q'].map(|scalar| Draw {
            clip: Rect {
                x: 0,
                y: 0,
                width: 1000,
                height: 1000,
            },
            primitive: Primitive::Glyph {
                x,
                y: 0,
                scalar,
                style: GlyphStyle {
                    ink: 0x102030,
                    background: 0xf0e0d0,
                    weight: Weight::Medium,
                },
            },
        })
    };
    let paint = |surface: Surface, face: Option<&mut Face>, typeface: Option<&mut Typeface>| {
        let mut pixels = vec![0x5a; surface.width * surface.height * 4];
        let mut raster = Raster::new(&mut pixels, &bitmap, surface, surface.width * 4).unwrap();
        if let Some(face) = face {
            raster = raster.with_face(face);
        }
        let mut raster = raster.with_typeface(typeface);
        for draw in draws(0).into_iter().chain(draws(40)) {
            raster.draw(draw);
        }
        pixels
    };
    let mut typeface = Typeface::new(font(), None).unwrap();
    for scale in [2, 1] {
        let surface = Surface::new(80, 40, Scale::new(scale).unwrap()).unwrap();
        let size = usize::from(scale);
        let mut fitted =
            Face::fit(font().into(), None, CELL_WIDTH * size, CELL_HEIGHT * size).unwrap();
        let expected = paint(surface, Some(&mut fitted), None);
        assert_ne!(expected, paint(surface, None, None), "scale {scale}");
        assert_eq!(
            paint(surface, None, Some(&mut typeface)),
            expected,
            "scale {scale}"
        );
    }
}

#[test]
fn the_bitmap_setting_keeps_a_program_on_the_bitmap_face() {
    assert_eq!(SETTING, "TD_UI_FACE");
    let dir = scratch("setting");
    fs::write(dir.join(REGULAR), font()).unwrap();
    let setting = |value: &str| Some(std::ffi::OsString::from(value));
    let places = [exact(&dir)];
    assert!(load_in_or_note(&places, "test", None).is_some());
    assert!(load_in_or_note(&places, "test", setting("outline").as_deref()).is_some());
    assert!(load_in_or_note(&places, "test", setting("bitmap").as_deref()).is_none());
    // Without the face the note is written and the program still starts.
    fs::remove_file(dir.join(REGULAR)).unwrap();
    assert!(load_in_or_note(&places, "test", None).is_none());
    fs::remove_dir_all(&dir).unwrap();
}

/// A style whose '0' is the square (`left`, `bottom`) to (`right`, `top`),
/// advancing 500 of 1000 units like the others, so the four fit one cell.
fn style(left: i32, bottom: i32, right: i32, top: i32) -> Vec<u8> {
    let square = vec![
        (left, bottom, true),
        (left, top, true),
        (right, top, true),
        (right, bottom, true),
    ];
    let mut builder = Builder::new(vec![Glyph::Empty, Glyph::Simple(vec![square])]);
    builder.advance = Some(500);
    builder.format4 = vec![Segment::Delta(0x30, 0x30, 1u16.wrapping_sub(0x30))];
    builder.font()
}

/// A terminal's four styles load as one face fitted to its cell unless the
/// setting asks for the bitmap face; a style missing is the whole face
/// refused, so the terminal draws in Unifont rather than in a face that
/// cannot draw every rendition.
#[test]
fn a_terminal_loads_four_styles_unless_the_setting_asks_for_the_bitmap_face() {
    let dir = scratch("styles");
    for (name, bytes) in [
        (REGULAR, style(0, 0, 500, 500)),
        (BOLD, style(0, 0, 250, 250)),
        (ITALIC, style(250, 0, 500, 250)),
        (BOLD_ITALIC, style(0, 250, 250, 500)),
    ] {
        fs::write(dir.join(name), bytes).unwrap();
    }
    let load = |setting: Option<&str>| {
        let setting = setting.map(std::ffi::OsString::from);
        styles_in_or_note(&[exact(&dir)], "test", CELL, setting.as_deref())
    };
    let loaded = load(None).expect("the four styles load");
    assert_eq!(
        (loaded.cell().width, loaded.cell().height),
        (CELL_WIDTH, CELL_HEIGHT)
    );
    assert_eq!(loaded.style(true, true), Style::BoldItalic);
    assert_eq!(loaded.style(false, true), Style::Italic);
    assert!(load(Some("outline")).is_some());
    assert!(load(Some("bitmap")).is_none());
    // Every style is read from the directory the search finds the regular
    // style in: an earlier-named one at the same depth holding only that
    // refuses the face.
    let base = dir
        .parent()
        .unwrap()
        .join(format!("styles-walk-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    std::os::unix::fs::symlink(&dir, base.join("face")).unwrap();
    let walked = [Place {
        dir: base.clone(),
        depth: 1,
    }];
    let find_styles = || styles_in_or_note(&walked, "test", CELL, None);
    assert!(find_styles().is_some());
    fs::create_dir_all(base.join("bare")).unwrap();
    fs::write(base.join("bare").join(REGULAR), style(0, 0, 500, 500)).unwrap();
    assert!(find_styles().is_none());
    fs::remove_dir_all(&base).unwrap();
    fs::remove_file(dir.join(ITALIC)).unwrap();
    assert!(load(None).is_none());
    fs::remove_dir_all(&dir).unwrap();
}

fn exact(dir: &Path) -> Place {
    Place {
        dir: dir.to_path_buf(),
        depth: 0,
    }
}

fn walked(dir: &Path) -> Place {
    Place {
        dir: dir.to_path_buf(),
        depth: SEARCH_DEPTH,
    }
}

fn face_in(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(REGULAR), font()).unwrap();
    dir.to_path_buf()
}

#[test]
fn the_places_are_the_image_the_install_and_the_font_roots_in_order() {
    fn os(value: &str) -> Option<&OsStr> {
        Some(OsStr::new(value))
    }
    let place = |dir: &str, depth: usize| Place {
        dir: dir.into(),
        depth,
    };
    assert_eq!(INSTALLED, "fonts/jetbrains-mono-nerd");
    assert_eq!(
        places(os("/h"), None, None),
        [
            place(DIR, 0),
            place("/h/.local/share/fonts/jetbrains-mono-nerd", 0),
            place("/h/.local/share/fonts", SEARCH_DEPTH),
            place("/h/.fonts", SEARCH_DEPTH),
            place("/usr/local/share/fonts", SEARCH_DEPTH),
            place("/usr/share/fonts", SEARCH_DEPTH),
        ]
    );
    // The XDG values when absolute, a repeat kept once.
    assert_eq!(
        places(os("/h"), os("/d"), os("/p/share:rel:/d:/usr/share")),
        [
            place(DIR, 0),
            place("/d/fonts/jetbrains-mono-nerd", 0),
            place("/d/fonts", SEARCH_DEPTH),
            place("/h/.fonts", SEARCH_DEPTH),
            place("/p/share/fonts", SEARCH_DEPTH),
            place("/usr/share/fonts", SEARCH_DEPTH),
        ]
    );
    // Relative or empty values are ignored; with no home only the image
    // and the system roots remain.
    assert_eq!(
        places(os("rel"), os("rel"), os("")),
        [
            place(DIR, 0),
            place("/usr/local/share/fonts", SEARCH_DEPTH),
            place("/usr/share/fonts", SEARCH_DEPTH),
        ]
    );
    assert_eq!(
        places(None, os("/d"), None),
        places(os("rel"), os("/d"), None)
    );
}

#[test]
fn the_search_takes_the_first_place_and_its_shallowest_first_named_face() {
    let base = scratch("search");
    let image = base.join("image");
    let installed = base.join("data/fonts/jetbrains-mono-nerd");
    let root = base.join("data/fonts");
    let system = base.join("system");
    // Nothing yet: the error names every place.
    fs::create_dir_all(&image).unwrap();
    fs::create_dir_all(&system).unwrap();
    let order = [
        exact(&image),
        exact(&installed),
        walked(&root),
        walked(&system),
    ];
    let missing = find(&order).unwrap_err();
    for place in &order {
        assert!(
            missing.contains(&place.dir.display().to_string()),
            "{missing}"
        );
    }
    assert!(missing.starts_with(REGULAR), "{missing}");
    // Under a root, the shallowest at any depth within bound, then the
    // first in name order, whatever order a directory lists them in.
    let deep = face_in(&system.join("b/c/d/e"));
    assert_eq!(find(&order).unwrap(), deep);
    let shallow = face_in(&system.join("z/x"));
    assert_eq!(find(&order).unwrap(), shallow);
    let named = face_in(&system.join("a/x"));
    assert_eq!(find(&order).unwrap(), named);
    let top = face_in(&system.join("y"));
    assert_eq!(find(&order).unwrap(), top);
    // A later place loses to an earlier one, a walked root to an exact
    // directory.
    let user = face_in(&root.join("TTF"));
    assert_eq!(find(&order).unwrap(), user);
    face_in(&installed);
    assert_eq!(find(&order).unwrap(), installed);
    face_in(&image);
    assert_eq!(find(&order).unwrap(), image);
    let typeface = load_in(&order);
    assert!(typeface.is_ok());
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn the_search_skips_hidden_and_too_deep_names_and_follows_a_linked_directory() {
    let base = scratch("bounds");
    let root = base.join("root");
    face_in(&root.join(".stage"));
    face_in(&root.join("1/2/3/4/5"));
    assert!(find(&[walked(&root)]).is_err());
    // A profile's linked font directory is followed, and a cycle is
    // bounded by the depth.
    let store = face_in(&base.join("store/font"));
    std::os::unix::fs::symlink(base.join("store"), root.join("profile")).unwrap();
    std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
    assert_eq!(find(&[walked(&root)]).unwrap(), root.join("profile/font"));
    assert!(store.join(REGULAR).is_file());
    // A face name that is not a regular file is not the face.
    let named = base.join("named");
    fs::create_dir_all(named.join(REGULAR)).unwrap();
    assert!(find(&[exact(&named)]).is_err());
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn the_search_stops_after_its_entry_budget_wherever_the_entries_are() {
    let base = scratch("budget");
    let wide = base.join("wide");
    fs::create_dir_all(&wide).unwrap();
    for n in 0..SEARCH_ENTRIES {
        fs::File::create(wide.join(format!("f{n:05}"))).unwrap();
    }
    let after = face_in(&base.join("after/x"));
    // The budget is spent on the wide root: a later root is not walked,
    // though an exact directory is still looked in.
    let stopped = find(&[walked(&wide), walked(&base.join("after"))]).unwrap_err();
    assert!(stopped.contains("stopped after"), "{stopped}");
    assert_eq!(
        find(&[walked(&wide), exact(&after)]).unwrap(),
        after,
        "an exact place costs no entry"
    );
    // One entry fewer and the later root is reached.
    fs::remove_file(wide.join("f00000")).unwrap();
    assert_eq!(
        find(&[walked(&wide), walked(&base.join("after"))]).unwrap(),
        after
    );
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn a_program_without_the_face_is_told_to_install_it() {
    assert_eq!(
        INSTALL_HINT,
        "run ./install-fonts from a td checkout to install it"
    );
}

#[test]
fn the_font_notice_names_the_directory_the_loader_reads() {
    assert!(OUTLINE_FACE.contains(&format!("read from {DIR},")));
}

#[test]
fn a_typeface_refuses_what_its_grid_face_refuses() {
    assert!(matches!(
        Typeface::new(vec![0; 3], None).unwrap_err(),
        Error::Truncated(_)
    ));
    assert!(Typeface::new(font(), Some(b"OTTO\0\0\0\0".to_vec())).is_err());
}

#[test]
fn the_loader_reads_the_regular_style_and_refuses_what_it_cannot_read() {
    assert_eq!(DIR, "/etc/fonts/jetbrains-mono-nerd");
    let dir = scratch("load");
    let path = dir.join(REGULAR).display().to_string();
    let missing = load_from(&dir).unwrap_err();
    assert!(missing.starts_with(&path), "{missing}");
    fs::write(dir.join(REGULAR), font()).unwrap();
    let mut typeface = load_from(&dir).unwrap();
    let cell = typeface.face(Scale::default()).unwrap().cell();
    assert_eq!((cell.width, cell.height), (CELL_WIDTH, CELL_HEIGHT));
    // Past the reader's bound is refused before a byte is read.
    fs::File::create(dir.join(REGULAR))
        .unwrap()
        .set_len(MAX_FONT_BYTES as u64 + 1)
        .unwrap();
    let large = load_from(&dir).unwrap_err();
    assert!(
        large.starts_with(&path) && large.contains("larger than"),
        "{large}"
    );
    fs::write(dir.join(REGULAR), b"not a font").unwrap();
    let refused = load_from(&dir).unwrap_err();
    assert!(refused.starts_with(&path), "{refused}");
    // Anything but a regular file is refused before it is opened.
    fs::remove_file(dir.join(REGULAR)).unwrap();
    fs::create_dir(dir.join(REGULAR)).unwrap();
    let directory = load_from(&dir).unwrap_err();
    assert!(directory.contains("not a regular file"), "{directory}");
    fs::remove_dir_all(&dir).unwrap();
}
