//! The pinned outline face as a widget program's `Typeface`, read through
//! `face_file`. A program that cannot read it draws with the bitmap face
//! and says so once; it never fails to start for want of it. A consumer
//! passes its `SETTING` value, as it passes the Wayland endpoint's, and
//! `bitmap` keeps it on the bitmap face without reading anything. Only the
//! regular style is read: the draw stream has no bold weight.

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;

pub use crate::face_file::SETTING;
use crate::face_file::{self, DIR, REGULAR};
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
