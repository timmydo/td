//! A directory only its owner can use: created 0700 when missing, and
//! refused unless it is a real directory this process owns with no group
//! or other permission bits.
//!
//! The owner is the uid of `/proc/self`, so no libc is needed: the kernel
//! gives that directory the process's effective uid while the process is
//! dumpable, and root's once it is not (after a setuid exec), which fails
//! closed for a user's directory.

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::Path;

/// Create `path` as a private directory if it is missing, along with any
/// missing parents (each 0700, less the umask), then require it to be
/// private (`check_private_dir`). An existing directory is never
/// loosened or tightened: a wrong one is refused.
pub fn private_dir(path: &Path) -> io::Result<()> {
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
    {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("create {}: {error}", path.display()),
            ))
        }
    }
    check_private_dir(path)
}

/// Require `path` to be a directory, not a symlink to one, owned by this
/// process's uid, with no group or other permission bits.
pub fn check_private_dir(path: &Path) -> io::Result<()> {
    let named =
        |error: io::Error| io::Error::new(error.kind(), format!("{}: {error}", path.display()));
    let metadata = std::fs::symlink_metadata(path).map_err(named)?;
    let uid = std::fs::metadata("/proc/self")
        .map_err(|error| io::Error::new(error.kind(), format!("/proc/self: {error}")))?
        .uid();
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be a private directory owned by this user (0700), not a symlink",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn a_missing_directory_and_its_parents_are_created_private() {
        let dir = scratch("private-create");
        let leaf = dir.join("a").join("b");
        private_dir(&leaf).unwrap();
        assert!(leaf.is_dir());
        assert_eq!(mode(&leaf) & 0o077, 0);
        assert_eq!(mode(&dir.join("a")) & 0o077, 0);
        // Again, over the directory it made.
        private_dir(&leaf).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_shared_directory_is_refused_and_left_as_it_was() {
        let dir = scratch("private-shared");
        let shared = dir.join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o750)).unwrap();
        let error = private_dir(&shared).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains(&shared.display().to_string()),
            "{error}"
        );
        assert_eq!(mode(&shared), 0o750);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_symlink_or_a_file_is_refused() {
        let dir = scratch("private-link");
        let real = dir.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(check_private_dir(&real).is_ok());
        assert!(private_dir(&link).is_err());
        let file = dir.join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(private_dir(&file).is_err());
        assert!(check_private_dir(&dir.join("absent")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
