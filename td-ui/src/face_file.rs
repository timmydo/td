//! The pinned outline face's files: the one directory both the image and a
//! jailed program's runtime give it (td-ui/DESIGN.md, "Delivery and trust
//! position"), its styles' names, and the bounded read of one of them.
//! td-term mounts this module beside the pure face modules, so it depends
//! on nothing else of td-ui's.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use crate::sfnt::MAX_FONT_BYTES;

pub const DIR: &str = "/etc/fonts/jetbrains-mono-nerd";
pub const REGULAR: &str = "JetBrainsMonoNerdFontMono-Regular.ttf";
pub const BOLD: &str = "JetBrainsMonoNerdFontMono-Bold.ttf";
pub const ITALIC: &str = "JetBrainsMonoNerdFontMono-Italic.ttf";
pub const BOLD_ITALIC: &str = "JetBrainsMonoNerdFontMono-BoldItalic.ttf";
/// The environment variable whose value a program passes to `wanted`;
/// `bitmap` is the one value that changes anything.
pub const SETTING: &str = "TD_UI_FACE";

/// Whether a program whose `SETTING` value is `setting` reads the face:
/// all but `bitmap` do.
pub fn wanted(setting: Option<&OsStr>) -> bool {
    setting != Some(OsStr::new("bitmap"))
}

/// The bytes of `name` in `dir`: a regular file of at most the reader's
/// bound, checked before it is opened, so a device or pipe named there is
/// refused rather than opened and an oversized file is never read. The
/// path is image content, so nothing swaps it between check and open.
/// Errors name the path.
pub fn read(dir: &Path, name: &str) -> Result<Vec<u8>, String> {
    let path = dir.join(name);
    let named = |why: &dyn std::fmt::Display| format!("{}: {why}", path.display());
    let metadata = fs::metadata(&path).map_err(|why| named(&why))?;
    if !metadata.is_file() {
        return Err(named(&"not a regular file"));
    }
    if metadata.len() > MAX_FONT_BYTES as u64 {
        return Err(named(&format!("larger than {MAX_FONT_BYTES} bytes")));
    }
    let mut bytes = Vec::new();
    File::open(&path)
        .map_err(|why| named(&why))?
        .take(MAX_FONT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|why| named(&why))?;
    if bytes.len() > MAX_FONT_BYTES {
        return Err(named(&format!("larger than {MAX_FONT_BYTES} bytes")));
    }
    Ok(bytes)
}
