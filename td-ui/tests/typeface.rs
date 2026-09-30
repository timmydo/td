#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The typeface over fonts `fonts` encodes: a face fitted to the grid's
//! cell at each scale, made once, and the refusal of a face no grid cell
//! fits; and the pinned face's loader over a directory the test writes.

mod fonts;

use std::fs;
use std::path::PathBuf;

use fonts::{Builder, Glyph, Segment};
use td_ui::atlas::{Slot, Style};
use td_ui::pinned_face::{load_from, DIR, REGULAR};
use td_ui::raster::Scale;
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
