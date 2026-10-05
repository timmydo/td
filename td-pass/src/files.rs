//! The files td-pass reaches beside its vault: folder listings for the
//! finder, an exported copy written whole into a chosen folder, and a
//! copy read for import. Only the vault's ciphertext crosses here; no
//! notebook text does.

use std::collections::BinaryHeap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use td_ui::finder;

/// The folder a finder opens on: `$HOME` when it is an absolute folder,
/// else the root.
pub fn start_folder() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute() && home.is_dir())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The folder a finder for a password store opens on: `pass`'s own,
/// `$PASSWORD_STORE_DIR` when it is an absolute folder, else
/// `~/.password-store`, else the finder's usual start. The finder lists
/// no hidden name, so the store's folder is where it starts.
pub fn store_folder() -> PathBuf {
    let folder = |path: PathBuf| (path.is_absolute() && path.is_dir()).then_some(path);
    std::env::var_os("PASSWORD_STORE_DIR")
        .and_then(|store| folder(PathBuf::from(store)))
        .or_else(|| {
            std::env::var_os("HOME")
                .and_then(|home| folder(PathBuf::from(home).join(".password-store")))
        })
        .unwrap_or_else(start_folder)
}

/// The most directory entries one listing examines, so a folder of very
/// many is read to a bound, not whole.
const EXAMINED: usize = 16 * finder::ENTRIES;

/// A folder for the finder: its subfolders, then its regular files, each
/// sorted with case aside, to the finder's bounds; the listing says when
/// it was cut short. A file is enabled only when `files` names the
/// largest one that may be chosen; otherwise files are shown, disabled,
/// so a folder is chosen knowing what it holds. A hidden name, an entry
/// that is neither a folder nor a regular file, and a name the finder
/// cannot show are left out.
pub fn list_folder(path: &Path, files: Option<u64>) -> Result<finder::Listing, String> {
    let named = |error: io::Error| format!("{}: {error}", path.display());
    // (a file, the name with case aside, the name, the size, a link): a
    // max-heap keeps the first ENTRIES in that order.
    let mut kept: BinaryHeap<(bool, String, String, u64, bool)> = BinaryHeap::new();
    let mut truncated = false;
    for (seen, entry) in fs::read_dir(path).map_err(named)?.enumerate() {
        if seen >= EXAMINED {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.')
            || name.len() > finder::NAME_BYTES
            || name.chars().any(char::is_control)
        {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let metadata = if kind.is_symlink() {
            fs::metadata(entry.path())
        } else {
            entry.metadata()
        };
        let Ok(metadata) = metadata else {
            continue;
        };
        if !metadata.is_dir() && !metadata.is_file() {
            continue;
        }
        kept.push((
            metadata.is_file(),
            name.to_lowercase(),
            name,
            metadata.len(),
            kind.is_symlink(),
        ));
        if kept.len() > finder::ENTRIES {
            kept.pop();
            truncated = true;
        }
    }
    let mut entries = Vec::with_capacity(kept.len());
    let mut bytes = 0usize;
    for (file, _, name, size, link) in kept.into_sorted_vec() {
        let entry = if file {
            let enabled = files.is_some_and(|ceiling| size <= ceiling);
            finder::Entry::new(&name, &size_text(size), finder::Kind::File, enabled)
        } else {
            let meta = if link { "link" } else { "" };
            finder::Entry::new(&name, meta, finder::Kind::Folder, true)
        };
        let Ok(entry) = entry else {
            continue;
        };
        let next = bytes.saturating_add(entry.name().len() + entry.meta().len());
        if next > finder::LISTING_BYTES {
            truncated = true;
            break;
        }
        bytes = next;
        entries.push(entry);
    }
    finder::Listing::new(&path.to_string_lossy(), entries, truncated)
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// A size as a row's meta: bytes, or KiB, MiB and GiB to a whole unit;
/// the largest fits the finder's meta bound.
fn size_text(bytes: u64) -> String {
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{} KiB", bytes / 1024),
        1_048_576..=1_073_741_823 => format!("{} MiB", bytes / 1_048_576),
        _ => format!("{} GiB", bytes / 1_073_741_824),
    }
}

/// Writes `bytes` as the new file `name` in `folder`, readable by this
/// account alone, and makes it durable. An existing file is never
/// replaced; a write that fails leaves no partial copy behind.
pub fn write_copy(folder: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = folder.join(name);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| match error.kind() {
            io::ErrorKind::AlreadyExists => {
                format!("{} already exists; it is not replaced", path.display())
            }
            _ => format!("{}: {error}", path.display()),
        })?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        // The partial copy is emptied through the file made, then its name
        // removed only while it still names that file: a folder moved or
        // replaced meanwhile keeps what it holds.
        let _ = file.set_len(0);
        let made = file.metadata().map(|made| (made.dev(), made.ino()));
        let named = fs::symlink_metadata(&path).map(|named| (named.dev(), named.ino()));
        if let (Ok(made), Ok(named)) = (made, named) {
            if made == named {
                let _ = fs::remove_file(&path);
            }
        }
        return Err(format!("{}: {error}", path.display()));
    }
    // The copy is whole and durable; its name is made durable where the
    // folder allows it. A folder that cannot be opened for reading or
    // synced (some network and FUSE file systems) keeps the copy.
    if let Ok(folder) = fs::File::open(folder) {
        let _ = folder.sync_all();
    }
    Ok(path)
}

/// `O_NONBLOCK` on Linux x86-64 and AArch64: opening a FIFO put in a
/// copy's place does not wait for a writer.
const NONBLOCK: i32 = 0o4000;

/// Reads a copy for import: a regular file of at most `ceiling` bytes,
/// read to one byte past it and refused there. The path is checked to be
/// a file before it is opened, and again once open, without waiting.
pub fn read_copy(path: &Path, ceiling: usize) -> Result<Vec<u8>, String> {
    let named = |error: io::Error| format!("{}: {error}", path.display());
    let not_file = || format!("{} is not a file", path.display());
    if !fs::metadata(path).map_err(named)?.is_file() {
        return Err(not_file());
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(NONBLOCK)
        .open(path)
        .map_err(named)?;
    if !file.metadata().map_err(named)?.is_file() {
        return Err(not_file());
    }
    let mut bytes = Vec::new();
    file.take(ceiling as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(named)?;
    if bytes.len() > ceiling {
        return Err(format!(
            "{} is larger than a notebook copy can be",
            path.display()
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fresh folder, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("td-pass-files-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_copy_is_written_new_and_private_and_never_over_another() {
        let scratch = Scratch::new("write");
        let path = write_copy(&scratch.0, "copy", b"sealed").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"sealed");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let again = write_copy(&scratch.0, "copy", b"other").unwrap_err();
        assert!(again.contains("not replaced"), "{again}");
        assert_eq!(fs::read(&path).unwrap(), b"sealed");
        assert!(write_copy(&scratch.0.join("absent"), "copy", b"x").is_err());
    }

    #[test]
    fn a_copy_is_read_to_its_bound_and_refused_past_it() {
        let scratch = Scratch::new("read");
        let path = scratch.0.join("copy");
        fs::write(&path, b"12345").unwrap();
        assert_eq!(read_copy(&path, 5).unwrap(), b"12345");
        assert!(read_copy(&path, 4).unwrap_err().contains("larger"));
        assert!(read_copy(&scratch.0, 5).is_err(), "a folder is no copy");
        assert!(read_copy(&scratch.0.join("absent"), 5).is_err());
    }

    #[test]
    fn a_listing_puts_folders_first_and_enables_files_by_size() {
        let scratch = Scratch::new("list");
        fs::create_dir(scratch.0.join("b-folder")).unwrap();
        fs::write(scratch.0.join("A-small"), b"1").unwrap();
        fs::write(scratch.0.join("c-large"), b"12345").unwrap();
        fs::write(scratch.0.join(".hidden"), b"1").unwrap();
        let shown = |listing: &finder::Listing| {
            listing
                .entries()
                .iter()
                .map(|entry| (entry.name().to_owned(), entry.enabled()))
                .collect::<Vec<_>>()
        };
        let choosing = list_folder(&scratch.0, Some(4)).unwrap();
        assert_eq!(
            shown(&choosing),
            [
                ("b-folder".to_owned(), true),
                ("A-small".to_owned(), true),
                ("c-large".to_owned(), false),
            ]
        );
        let folders = list_folder(&scratch.0, None).unwrap();
        assert!(folders
            .entries()
            .iter()
            .all(|entry| entry.enabled() == (entry.kind() == finder::Kind::Folder)));
        assert!(list_folder(&scratch.0.join("absent"), None).is_err());
    }
}
