//! The pinned outline face's files: the one directory both the image and a
//! jailed program's runtime give it (td-ui/DESIGN.md, "Delivery and trust
//! position"), the host directories a program run outside the image looks
//! in, its styles' names, and the bounded read of one of them.
//! `pinned_face` reads through it.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::sfnt::MAX_FONT_BYTES;

pub const DIR: &str = "/etc/fonts/jetbrains-mono-nerd";
/// The same directory under a host user's XDG data home, where
/// `./install-fonts` puts it.
pub const INSTALLED: &str = "fonts/jetbrains-mono-nerd";
pub const REGULAR: &str = "JetBrainsMonoNerdFontMono-Regular.ttf";
pub const BOLD: &str = "JetBrainsMonoNerdFontMono-Bold.ttf";
pub const ITALIC: &str = "JetBrainsMonoNerdFontMono-Italic.ttf";
pub const BOLD_ITALIC: &str = "JetBrainsMonoNerdFontMono-BoldItalic.ttf";
/// The environment variable whose value a program passes to `wanted`;
/// `bitmap` is the one value that changes anything.
pub const SETTING: &str = "TD_UI_FACE";
/// What a program that draws with Unifont for want of the face says to do.
pub const INSTALL_HINT: &str = "run ./install-fonts from a td checkout to install it";
/// How many directories deep the search looks under a font root.
pub const SEARCH_DEPTH: usize = 4;
/// How many directory entries the search reads in all before it stops.
pub const SEARCH_ENTRIES: usize = 16_384;
/// Linux's `O_NONBLOCK` on x86-64 and aarch64: opening a FIFO for reading
/// returns at once rather than waiting for a writer.
const O_NONBLOCK: i32 = 0o4000;

/// Whether a program whose `SETTING` value is `setting` reads the face:
/// all but `bitmap` do.
pub fn wanted(setting: Option<&OsStr>) -> bool {
    setting != Some(OsStr::new("bitmap"))
}

/// A directory the search looks in, and how many levels below it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub dir: PathBuf,
    pub depth: usize,
}

/// Where the face is looked for, in order, given `HOME`, `XDG_DATA_HOME`
/// and `XDG_DATA_DIRS`: `DIR`; `INSTALLED` under the data home
/// (`XDG_DATA_HOME`, else `HOME/.local/share`); then, `SEARCH_DEPTH` deep,
/// the font roots: the data home's `fonts`, `HOME/.fonts`, and `fonts`
/// under each data directory (`XDG_DATA_DIRS`, else `/usr/local/share`
/// and `/usr/share`). A relative or empty `HOME` or `XDG_DATA_HOME` is
/// ignored, as is a relative entry of `XDG_DATA_DIRS`, as the XDG base
/// directory specification says; its defaults stand only for an unset or
/// empty list. A repeated place is kept once.
pub fn places(
    home: Option<&OsStr>,
    data_home: Option<&OsStr>,
    data_dirs: Option<&OsStr>,
) -> Vec<Place> {
    fn absolute(value: &OsStr) -> Option<&Path> {
        Some(Path::new(value)).filter(|path| path.is_absolute())
    }
    let home = home.and_then(absolute);
    let data_home = data_home
        .and_then(absolute)
        .map(Path::to_path_buf)
        .or_else(|| home.map(|home| home.join(".local/share")));
    let data_dirs: Vec<PathBuf> = match data_dirs.filter(|dirs| !dirs.is_empty()) {
        Some(dirs) => std::env::split_paths(dirs)
            .filter(|dir| dir.is_absolute())
            .collect(),
        None => vec!["/usr/local/share".into(), "/usr/share".into()],
    };
    let exact = [
        Some(PathBuf::from(DIR)),
        data_home.as_ref().map(|d| d.join(INSTALLED)),
    ];
    let roots = [
        data_home.as_ref().map(|d| d.join("fonts")),
        home.map(|home| home.join(".fonts")),
    ];
    let mut out: Vec<Place> = Vec::new();
    let found = exact
        .into_iter()
        .flatten()
        .map(|dir| Place { dir, depth: 0 })
        .chain(
            roots
                .into_iter()
                .flatten()
                .chain(data_dirs.iter().map(|dir| dir.join("fonts")))
                .map(|dir| Place {
                    dir,
                    depth: SEARCH_DEPTH,
                }),
        );
    for place in found {
        if !out.iter().any(|seen| seen.dir == place.dir) {
            out.push(place);
        }
    }
    out
}

/// `places` from this process's environment.
pub fn host_places() -> Vec<Place> {
    places(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("XDG_DATA_DIRS").as_deref(),
    )
}

/// The first directory that holds `REGULAR` as a regular file: the places
/// in order, each walked a level at a time, so a shallower directory wins,
/// and each level in its parents' order and then name order. The walk reads
/// at most `SEARCH_ENTRIES` directory entries in all and skips hidden
/// names, and one entry past that bound ends every walk; it follows a
/// symbolic link to a directory, as a Guix or Nix profile's font
/// directories are, the depth bounding a cycle. The answer names the
/// face's directory and nothing is read from it here.
pub fn find(places: &[Place]) -> Result<PathBuf, String> {
    let mut budget = Some(SEARCH_ENTRIES);
    for place in places {
        if let Some(dir) = search(&place.dir, place.depth, &mut budget) {
            return Ok(dir);
        }
    }
    let named: Vec<String> = places
        .iter()
        .map(|place| place.dir.display().to_string())
        .collect();
    let stopped = match budget {
        Some(_) => String::new(),
        None => format!(" (stopped after {SEARCH_ENTRIES} entries)"),
    };
    Err(format!(
        "{REGULAR} is in none of {}{stopped}",
        named.join(", ")
    ))
}

/// The first directory at most `depth` levels under `root`, `root` itself
/// first, that holds `REGULAR`. A directory with more entries than the
/// budget has left spends it, `None` thereafter, which ends this walk and
/// every later one, so where the search stops does not depend on the order
/// a directory lists its entries in.
fn search(root: &Path, depth: usize, budget: &mut Option<usize>) -> Option<PathBuf> {
    let mut level = vec![root.to_path_buf()];
    for below in (0..=depth).rev() {
        if let Some(dir) = level
            .iter()
            .find(|dir| fs::metadata(dir.join(REGULAR)).is_ok_and(|m| m.is_file()))
        {
            return Some(dir.clone());
        }
        if below == 0 {
            break;
        }
        let mut next = Vec::new();
        for dir in &level {
            next.extend(subdirs(dir, budget)?);
        }
        level = next;
    }
    None
}

/// `dir`'s directories that are not hidden, in name order, each entry read
/// taken from the budget; none, with the budget spent, when it runs out,
/// and none at all once it is spent: the entry that finds it out is the
/// one read past it.
fn subdirs(dir: &Path, budget: &mut Option<usize>) -> Option<Vec<PathBuf>> {
    budget.as_ref()?;
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return Some(out);
    };
    for entry in entries {
        *budget = budget.and_then(|left| left.checked_sub(1));
        budget.as_ref()?;
        let Ok(entry) = entry else { continue };
        if entry.file_name().as_encoded_bytes().starts_with(b".") {
            continue;
        }
        let directory = match entry.file_type() {
            Ok(kind) if kind.is_dir() => true,
            Ok(kind) if kind.is_symlink() => entry.path().is_dir(),
            _ => false,
        };
        if directory {
            out.push(entry.path());
        }
    }
    out.sort();
    Some(out)
}

/// The bytes of `name` in `dir`: a regular file of at most the reader's
/// bound, checked before it is opened, so a device or pipe named there is
/// refused rather than opened and an oversized file is never read, and
/// checked again once open, so a file swapped for another between the two
/// is refused too. The open does not wait, so a pipe swapped in cannot
/// hold a program's start. Errors name the path.
pub fn read(dir: &Path, name: &str) -> Result<Vec<u8>, String> {
    let path = dir.join(name);
    let named = |why: &dyn std::fmt::Display| format!("{}: {why}", path.display());
    let check = |metadata: fs::Metadata| {
        if !metadata.is_file() {
            return Err(named(&"not a regular file"));
        }
        if metadata.len() > MAX_FONT_BYTES as u64 {
            return Err(named(&format!("larger than {MAX_FONT_BYTES} bytes")));
        }
        Ok(())
    };
    check(fs::metadata(&path).map_err(|why| named(&why))?)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(&path)
        .map_err(|why| named(&why))?;
    check(file.metadata().map_err(|why| named(&why))?)?;
    let mut bytes = Vec::new();
    file.take(MAX_FONT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|why| named(&why))?;
    if bytes.len() > MAX_FONT_BYTES {
        return Err(named(&format!("larger than {MAX_FONT_BYTES} bytes")));
    }
    Ok(bytes)
}
