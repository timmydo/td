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
//! scale; and the pinned face's loader over a directory the test writes.

mod fonts;

use std::fs;
use std::path::PathBuf;

use fonts::{Builder, Glyph, Segment};
use td_ui::atlas::{Slot, Style};
use td_ui::face::Face;
use td_ui::font::pinned;
use td_ui::notices::OUTLINE_FACE;
use td_ui::pinned_face::{load_from, load_from_or_note, DIR, REGULAR, SETTING};
use td_ui::raster::{Draw, GlyphStyle, Primitive, Raster, Rect, Scale, Surface, Weight};
use td_ui::sfnt::{Error, MAX_FONT_BYTES};
use td_ui::typeface::Typeface;
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

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
    assert!(load_from_or_note(&dir, "test", None).is_some());
    assert!(load_from_or_note(&dir, "test", setting("outline").as_deref()).is_some());
    assert!(load_from_or_note(&dir, "test", setting("bitmap").as_deref()).is_none());
    // Without the face the note is written and the program still starts.
    fs::remove_file(dir.join(REGULAR)).unwrap();
    assert!(load_from_or_note(&dir, "test", None).is_none());
    fs::remove_dir_all(&dir).unwrap();
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
