//! Replacing a file whole, durably, through a synced temporary beside it.
//!
//! The one shape behind what td programs wrote as their own `write_atomic`,
//! `replace` or `publish`: a reader, or the file after a crash, holds the
//! old bytes or the new and never part of either.

use std::fs::File;
use std::io;
use std::path::Path;

/// Tells apart the temporaries of concurrent `replace` calls in one process;
/// the pid in the name tells apart processes.
pub(crate) static REPLACE_SERIAL: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many names `replace` tries before giving up. A name is taken only by
/// a temporary a crashed process left under a pid this one now has, so the
/// next serial is all but always free; the bound keeps a directory full of
/// them from looping forever.
const NAME_ATTEMPTS: u32 = 64;

/// Replace `path` whole with `bytes`, durably: a fresh temporary beside it,
/// created exclusively with `mode` (less the umask), written and synced,
/// renamed over `path`, and the directory synced. A reader, or the file
/// after a crash, holds the old bytes or the new and never part of either,
/// and once this returns the new bytes survive a crash. The temporary is
/// removed when a step fails. A symlink at `path` is replaced, not
/// followed, which is what `rename` does.
///
/// The directory is opened first, so one this cannot open for its sync
/// fails before anything is written. Only the sync itself comes after the
/// rename: an error from it means the new bytes are in place but may not
/// survive a crash. A filesystem that has no directory sync (EINVAL) is
/// taken as synced. A temporary name that is taken, left by a crash, is
/// passed over for the next, never removed.
pub fn replace(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    replace_with(path, bytes, mode, |_| Ok(()))
}

/// Set and check a sensitive file's attributes on its new private inode
/// before writing or publishing it. A failed check preserves the old file.
pub fn replace_with(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    prepare: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<()> {
    use std::io::Write;
    let directory = File::open(parent_of(path)).map_err(|e| named(path, "", e))?;
    stream_with_sync(
        &directory,
        path,
        mode,
        |file| {
            prepare(file)?;
            file.write_all(bytes)
        },
        |temporary| std::fs::rename(temporary, path),
        File::sync_all,
    )
}

fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn named(path: &Path, stage: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}{stage}: {error}", path.display()))
}

pub(crate) fn stream_with_sync(
    directory: &File,
    path: &Path,
    mode: u32,
    write: impl FnOnce(&mut File) -> io::Result<()>,
    publish: impl FnOnce(&Path) -> io::Result<()>,
    sync: impl Fn(&File) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let fail = |e| named(path, "", e);
    let name = path
        .file_name()
        .ok_or_else(|| fail(io::ErrorKind::InvalidInput.into()))?;
    let parent = parent_of(path);
    let mut attempt = 0;
    let (temporary, mut file) = loop {
        let serial = REPLACE_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let leaf = format!(".td-replace.{}.{serial}.tmp", std::process::id());
        if name == std::ffi::OsStr::new(&leaf) {
            continue;
        }
        let temporary = parent.join(leaf);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                attempt += 1;
                if attempt >= NAME_ATTEMPTS {
                    return Err(fail(error));
                }
            }
            Err(error) => return Err(fail(error)),
        }
    };
    use std::os::unix::fs::MetadataExt;
    let same_file = |file: &File| -> io::Result<()> {
        let opened = file.metadata()?;
        let named = std::fs::symlink_metadata(&temporary)?;
        if (opened.dev(), opened.ino()) != (named.dev(), named.ino()) {
            return Err(io::Error::other("staging file changed"));
        }
        Ok(())
    };
    let placed = write(&mut file)
        .and_then(|()| sync(&file))
        .and_then(|()| same_file(&file))
        .and_then(|()| publish(&temporary));
    if let Err(error) = placed {
        if same_file(&file).is_ok() {
            let _ = std::fs::remove_file(&temporary);
        }
        return Err(fail(error));
    }
    match sync(directory) {
        // EINVAL: a filesystem with no directory sync, so nothing to wait for.
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(()),
        synced => synced.map_err(|e| named(path, " published, directory sync failed", e)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "td-fs-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// What `replace` left in `dir` besides the files a test put there.
    fn leftovers(dir: &std::path::Path, expected: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !expected.contains(&name.as_str()))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn replace_creates_then_replaces_whole_with_the_mode_and_no_temporary() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("replace");
        let path = dir.join("state");
        replace(&path, b"first", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        replace(&path, b"second, longer", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second, longer");
        replace(&path, b"", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert!(leftovers(&dir, &["state"]).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replace_replaces_a_symlink_rather_than_writing_through_it() {
        let dir = scratch("replace-link");
        let target = dir.join("target");
        std::fs::write(&target, b"untouched").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        replace(&link, b"new", 0o644).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_file());
        assert_eq!(std::fs::read(&link).unwrap(), b"new");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_that_cannot_be_opened_fails_before_anything_is_written() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("replace-unreadable");
        let inner = dir.join("drafts");
        std::fs::create_dir(&inner).unwrap();
        let path = inner.join("state");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o300)).unwrap();
        let opened = File::open(&inner).is_ok();
        let result = replace(&path, b"new", 0o600);
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Root opens any directory; there the replace simply succeeds.
        if opened {
            result.unwrap();
        } else {
            let error = result.unwrap_err();
            assert!(
                error.to_string().contains(&path.display().to_string()),
                "{error}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"old");
            assert!(leftovers(&inner, &["state"]).is_empty());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_replace_names_the_path_and_leaves_the_old_bytes() {
        let dir = scratch("replace-fail");
        let missing = dir.join("absent").join("state");
        let error = replace(&missing, b"x", 0o600).unwrap_err();
        assert!(
            error.to_string().contains(&missing.display().to_string()),
            "{error}"
        );
        // A directory where the file goes: the temporary is written, the
        // rename fails, and the temporary must not stay behind.
        let occupied = dir.join("occupied");
        std::fs::create_dir(&occupied).unwrap();
        std::fs::write(occupied.join("inside"), b"kept").unwrap();
        let error = replace(&occupied, b"x", 0o600).unwrap_err();
        assert!(
            error.to_string().contains(&occupied.display().to_string()),
            "{error}"
        );
        assert_eq!(std::fs::read(occupied.join("inside")).unwrap(), b"kept");
        assert!(leftovers(&dir, &["occupied"]).is_empty());
        assert!(replace(std::path::Path::new("/"), b"x", 0o600).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
