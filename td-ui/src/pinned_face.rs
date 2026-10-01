//! The pinned outline face as a widget program's `Typeface`, read through
//! `face_file` from the first place it finds the face. A program that
//! cannot read it draws with the bitmap face and says so once, naming
//! `./install-fonts`; it never fails to start for want of it. A consumer
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
use crate::face_file::{self, Place, BOLD, BOLD_ITALIC, INSTALL_HINT, ITALIC, REGULAR};
use crate::typeface::Typeface;

/// The face's regular style from the first of `face_file::host_places`
/// that holds it.
pub fn load() -> Result<Typeface, String> {
    load_in(&face_file::host_places())
}

/// The regular style from the first of `places` that holds it.
pub fn load_in(places: &[Place]) -> Result<Typeface, String> {
    load_from(&face_file::find(places)?)
}

/// The regular style from `dir`, read as `face_file::read` reads it; a
/// refused face is an error naming the path.
pub fn load_from(dir: &Path) -> Result<Typeface, String> {
    let bytes = face_file::read(dir, REGULAR)?;
    Typeface::new(bytes, None).map_err(|why| format!("{}: {why}", dir.join(REGULAR).display()))
}

/// `load`, or a line on standard error naming `program`, why it draws
/// with the bitmap face instead and how to install the face; a failed
/// write is ignored, never a panic. A `setting` of `bitmap` asks for the
/// bitmap face: nothing is read and nothing is said.
pub fn load_or_note(program: &str, setting: Option<&OsStr>) -> Option<Typeface> {
    if !face_file::wanted(setting) {
        return None;
    }
    load_in_or_note(&face_file::host_places(), program, setting)
}

/// `load_or_note` from `places`.
pub fn load_in_or_note(
    places: &[Place],
    program: &str,
    setting: Option<&OsStr>,
) -> Option<Typeface> {
    if !face_file::wanted(setting) {
        return None;
    }
    load_in(places).map_err(|why| note(program, &why)).ok()
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

/// The four styles from the first of `places` that holds the regular
/// style: every style is read from that one directory.
pub fn styles_in(places: &[Place], width: usize, height: usize) -> Result<Face, String> {
    styles_from(&face_file::find(places)?, width, height)
}

/// `styles_in` `face_file::host_places`, or a line on standard error
/// naming `program`, why it draws with the bitmap face instead and how to
/// install the face, as `load_or_note` says it; a `setting` of `bitmap`
/// reads and says nothing. A missing style is the whole face refused: a
/// terminal draws in Unifont rather than in a face that cannot draw every
/// rendition.
pub fn styles_or_note(
    program: &str,
    width: usize,
    height: usize,
    setting: Option<&OsStr>,
) -> Option<Face> {
    if !face_file::wanted(setting) {
        return None;
    }
    styles_in_or_note(&face_file::host_places(), program, width, height, setting)
}

/// `styles_or_note` from `places`.
pub fn styles_in_or_note(
    places: &[Place],
    program: &str,
    width: usize,
    height: usize,
    setting: Option<&OsStr>,
) -> Option<Face> {
    if !face_file::wanted(setting) {
        return None;
    }
    styles_in(places, width, height)
        .map_err(|why| note(program, &why))
        .ok()
}

/// The one line a program that draws with Unifont for want of the face
/// says; a failed write is ignored.
fn note(program: &str, why: &str) {
    let _ = writeln!(
        std::io::stderr(),
        "{program}: outline face unavailable ({why}); using Unifont: {INSTALL_HINT}"
    );
}
