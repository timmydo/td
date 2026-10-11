//! The pinned outline face as a widget program's `Typeface`, read through
//! `face_file` from the first place it finds the face. A program that
//! cannot read it draws with the bitmap face and says so once, naming
//! `./install-fonts`; it never fails to start for want of it. A consumer
//! passes its `SETTING` value, as it passes the Wayland endpoint's, and
//! `bitmap` keeps it on the bitmap face without reading anything. A widget
//! program reads the regular style alone unless it draws bold or italic
//! glyphs, when it reads all four (`load_styled_or_note`); a terminal reads
//! all four as one `Face`, fitted to its cell or at a size of its own
//! (`styles_or_note`), which `vt_render::render_with` draws through.

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;

use crate::face::{Face, Sizing};
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

/// The four styles from `dir` as a widget program's `Typeface`, for one
/// whose glyphs are bold or italic; a refused face is an error naming the
/// directory and the pair of styles the refusing step read.
pub fn load_styled_from(dir: &Path) -> Result<Typeface, String> {
    four_styles(dir, |[regular, bold, italic, bold_italic]| {
        let upright = Typeface::new(regular, Some(bold)).map_err(|why| ([REGULAR, BOLD], why))?;
        upright
            .with_slant(Some(italic), Some(bold_italic))
            .map_err(|why| ([ITALIC, BOLD_ITALIC], why))
    })
}

/// `load_styled_from` the first of `places` that holds the regular style:
/// every style is read from that one directory.
pub fn load_styled_in(places: &[Place]) -> Result<Typeface, String> {
    load_styled_from(&face_file::find(places)?)
}

/// `load_or_note` reading the four styles: a missing style is the whole
/// face refused, as `styles_or_note` says, so the program's styled text
/// never draws upright in an outline face beside a bitmap fallback.
pub fn load_styled_or_note(program: &str, setting: Option<&OsStr>) -> Option<Typeface> {
    load_styled_in_or_note(&face_file::host_places(), program, setting)
}

/// `load_styled_or_note` from `places`.
pub fn load_styled_in_or_note(
    places: &[Place],
    program: &str,
    setting: Option<&OsStr>,
) -> Option<Typeface> {
    if !face_file::wanted(setting) {
        return None;
    }
    load_styled_in(places)
        .map_err(|why| note(program, &why))
        .ok()
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

/// The pinned face's four styles from `dir`, sized as `sizing` asks; a
/// refused face is an error naming the directory and the pair of styles
/// the refusing step read.
pub fn styles_from(dir: &Path, sizing: Sizing) -> Result<Face, String> {
    four_styles(dir, |[regular, bold, italic, bold_italic]| {
        let upright = Face::with_sizing(regular.into(), Some(bold.into()), sizing)
            .map_err(|why| ([REGULAR, BOLD], why))?;
        upright
            .with_slant(Some(italic.into()), Some(bold_italic.into()))
            .map_err(|why| ([ITALIC, BOLD_ITALIC], why))
    })
}

/// The four styles' bytes from `dir`, read in turn, made into a face by
/// `make`, whose refusal names the pair of styles the refusing step read;
/// an error names the directory.
fn four_styles<T>(
    dir: &Path,
    make: impl FnOnce([Vec<u8>; 4]) -> Result<T, ([&'static str; 2], crate::sfnt::Error)>,
) -> Result<T, String> {
    let read = |name: &str| face_file::read(dir, name);
    let bytes = [
        read(REGULAR)?,
        read(BOLD)?,
        read(ITALIC)?,
        read(BOLD_ITALIC)?,
    ];
    make(bytes)
        .map_err(|([first, second], why)| format!("{} ({first} or {second}): {why}", dir.display()))
}

/// The four styles from the first of `places` that holds the regular
/// style: every style is read from that one directory.
pub fn styles_in(places: &[Place], sizing: Sizing) -> Result<Face, String> {
    styles_from(&face_file::find(places)?, sizing)
}

/// `styles_in` `face_file::host_places`, or a line on standard error
/// naming `program`, why it draws with the bitmap face instead and how to
/// install the face, as `load_or_note` says it; a `setting` of `bitmap`
/// reads and says nothing. A missing style is the whole face refused: a
/// terminal draws in Unifont rather than in a face that cannot draw every
/// rendition.
pub fn styles_or_note(program: &str, sizing: Sizing, setting: Option<&OsStr>) -> Option<Face> {
    if !face_file::wanted(setting) {
        return None;
    }
    styles_in_or_note(&face_file::host_places(), program, sizing, setting)
}

/// `styles_or_note` from `places`.
pub fn styles_in_or_note(
    places: &[Place],
    program: &str,
    sizing: Sizing,
    setting: Option<&OsStr>,
) -> Option<Face> {
    if !face_file::wanted(setting) {
        return None;
    }
    styles_in(places, sizing)
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
