//! The pinned outline face as a widget program's `Typeface`, read through
//! `face_file`. A program that cannot read it draws with the bitmap face
//! and says so once; it never fails to start for want of it. A consumer
//! passes its `SETTING` value, as it passes the Wayland endpoint's, and
//! `bitmap` keeps it on the bitmap face without reading anything. A widget
//! program reads the regular style alone, since the draw stream has no bold
//! weight; a terminal reads all four as one `Face` fitted to its cell
//! (`styles_or_note`), which `vt_render::render_with` draws through.

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use crate::face::Face;
pub use crate::face_file::SETTING;
use crate::face_file::{self, BOLD, BOLD_ITALIC, DIR, ITALIC, REGULAR};
use crate::typeface::Typeface;

/// The pinned face's regular style from `face_file::DIR`.
pub fn load() -> Result<Typeface, String> {
    load_from(Path::new(DIR))
}

/// The regular style from `dir`, read as `face_file::read` reads it; a
/// refused face is an error naming the path.
pub fn load_from(dir: &Path) -> Result<Typeface, String> {
    let bytes = face_file::read(dir, REGULAR)?;
    Typeface::new(bytes, None).map_err(|why| format!("{}: {why}", dir.join(REGULAR).display()))
}

/// `load`, or a line on standard error naming `program` and why it draws
/// with the bitmap face instead; a failed write is ignored, never a panic.
/// A `setting` of `bitmap` asks for the bitmap face: nothing is read and
/// nothing is said.
pub fn load_or_note(program: &str, setting: Option<&OsStr>) -> Option<Typeface> {
    load_from_or_note(Path::new(DIR), program, setting)
}

/// `load_or_note` from `dir`.
pub fn load_from_or_note(dir: &Path, program: &str, setting: Option<&OsStr>) -> Option<Typeface> {
    if !face_file::wanted(setting) {
        return None;
    }
    load_from(dir)
        .map_err(|why| {
            let _ = writeln!(
                std::io::stderr(),
                "{program}: outline face unavailable ({why}); using Unifont"
            );
        })
        .ok()
}

/// The pinned face's four styles from `dir`, fitted to a `width` by
/// `height` cell; a refused face is an error naming the directory and the
/// pair of styles the refusing step read.
pub fn styles_from(dir: &Path, width: usize, height: usize) -> Result<Face, String> {
    let read = |name: &str| face_file::read(dir, name).map(Arc::<[u8]>::from);
    let refused = |styles: [&str; 2], why: crate::sfnt::Error| {
        let [first, second] = styles;
        format!("{} ({first} or {second}): {why}", dir.display())
    };
    Face::fit(read(REGULAR)?, Some(read(BOLD)?), width, height)
        .map_err(|why| refused([REGULAR, BOLD], why))?
        .with_slant(Some(read(ITALIC)?), Some(read(BOLD_ITALIC)?))
        .map_err(|why| refused([ITALIC, BOLD_ITALIC], why))
}

/// `styles_from` `face_file::DIR`, or a line on standard error naming
/// `program` and why it draws with the bitmap face instead, as
/// `load_or_note` says it; a `setting` of `bitmap` reads and says nothing.
/// A missing style is the whole face refused: a terminal draws in Unifont
/// rather than in a face that cannot draw every rendition.
pub fn styles_or_note(
    program: &str,
    width: usize,
    height: usize,
    setting: Option<&OsStr>,
) -> Option<Face> {
    styles_from_or_note(Path::new(DIR), program, width, height, setting)
}

/// `styles_or_note` from `dir`.
pub fn styles_from_or_note(
    dir: &Path,
    program: &str,
    width: usize,
    height: usize,
    setting: Option<&OsStr>,
) -> Option<Face> {
    if !face_file::wanted(setting) {
        return None;
    }
    styles_from(dir, width, height)
        .map_err(|why| {
            let _ = writeln!(
                std::io::stderr(),
                "{program}: outline face unavailable ({why}); using Unifont"
            );
        })
        .ok()
}
