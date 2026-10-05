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
static REPLACE_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let named =
        |error: io::Error| io::Error::new(error.kind(), format!("{}: {error}", path.display()));
    let name = path
        .file_name()
        .ok_or_else(|| named(io::Error::from(io::ErrorKind::InvalidInput)))?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let directory = File::open(parent).map_err(named)?;
    let mut attempt = 0;
    let (temporary, mut file) = loop {
        let serial = REPLACE_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut temporary = std::ffi::OsString::from(".");
        temporary.push(name);
        temporary.push(format!(".{}.{serial}.tmp", std::process::id()));
        let temporary = parent.join(temporary);
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
                    return Err(named(error));
                }
            }
            Err(error) => return Err(named(error)),
        }
    };
    let placed = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&temporary, path));
    if let Err(error) = placed {
        let _ = std::fs::remove_file(&temporary);
        return Err(named(error));
    }
    match directory.sync_all() {
        // EINVAL: a filesystem with no directory sync, so nothing to wait for.
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(()),
        synced => synced.map_err(named),
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
    fn a_temporary_left_by_a_crash_is_passed_over_not_removed() {
        let dir = scratch("replace-squat");
        let path = dir.join("state");
        // The names the next calls in this process would take, as a crashed
        // process with this pid would have left them. Tests in other
        // threads take serials too, which only moves this one further on.
        let next = REPLACE_SERIAL.load(std::sync::atomic::Ordering::Relaxed);
        let squatters: Vec<_> = (next..next + 8)
            .map(|serial| dir.join(format!(".state.{}.{serial}.tmp", std::process::id())))
            .collect();
        for squatter in &squatters {
            std::fs::write(squatter, b"left").unwrap();
        }
        replace(&path, b"placed", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"placed");
        for squatter in &squatters {
            assert_eq!(std::fs::read(squatter).unwrap(), b"left");
        }
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
