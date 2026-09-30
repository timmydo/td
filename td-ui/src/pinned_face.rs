//! The pinned outline face, read from the one path both the image and a
//! jailed program's runtime give it (td-ui/DESIGN.md, "Delivery and trust
//! position"). A program that cannot read it draws with the bitmap face
//! and says so once; it never fails to start for want of it. Only the
//! regular style is read: the draw stream has no bold weight yet.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use crate::sfnt::MAX_FONT_BYTES;
use crate::typeface::Typeface;

pub const DIR: &str = "/etc/fonts/jetbrains-mono-nerd";
pub const REGULAR: &str = "JetBrainsMonoNerdFontMono-Regular.ttf";

/// The pinned face's regular style from `DIR`.
pub fn load() -> Result<Typeface, String> {
    load_from(Path::new(DIR))
}

/// The regular style from `dir`: a regular file of at most the reader's
/// bound, checked before it is opened, so a device or pipe named there is
/// refused rather than opened and an oversized file is never read. The
/// path is image content, so nothing swaps it between check and open.
pub fn load_from(dir: &Path) -> Result<Typeface, String> {
    let path = dir.join(REGULAR);
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
    Typeface::new(bytes, None).map_err(|why| named(&why))
}

/// `load`, or a line on standard error naming `program` and why it draws
/// with the bitmap face instead; a failed write is ignored, never a panic.
pub fn load_or_note(program: &str) -> Option<Typeface> {
    load()
        .map_err(|why| {
            let _ = writeln!(
                std::io::stderr(),
                "{program}: outline face unavailable ({why}); using Unifont"
            );
        })
        .ok()
}
