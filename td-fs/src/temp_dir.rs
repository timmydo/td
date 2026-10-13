//! Owned disposable directories for test fixtures and trusted scratch.
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Removes its uniquely claimed directory on drop, including mode-000 trees.
/// The exposed fixture is a child, so sibling lock files are owned too and
/// recreating the fixture never relinquishes the unique claim.
/// Only use for trees owned by the caller, with no concurrent writers.
#[derive(Debug)]
pub struct TempDir {
    owned: Option<PathBuf>,
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> io::Result<Self> {
        if tag.is_empty() || tag.contains('/') || tag.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid scratch tag",
            ));
        }
        for _ in 0..1024 {
            let path = std::env::temp_dir().join(format!(
                "{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    let fixture = path.join("d");
                    if let Err(e) = fs::DirBuilder::new().mode(0o700).create(&fixture) {
                        let _ = fs::remove_dir(&path);
                        return Err(e);
                    }
                    return Ok(Self {
                        owned: Some(path),
                        path: fixture,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "scratch names exhausted",
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Explicit cleanup can report failures; drop reports them to stderr.
    pub fn cleanup(mut self) -> io::Result<()> {
        match self.owned.take() {
            Some(path) => remove(&path),
            None => Ok(()),
        }
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<std::ffi::OsStr> for TempDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.path.as_os_str()
    }
}

fn remove(path: &Path) -> io::Result<()> {
    let mut pending = vec![(path.to_path_buf(), false)];
    while let Some((path, visited)) = pending.pop() {
        if visited {
            fs::remove_dir(path)?;
            continue;
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if !meta.is_dir() {
            fs::remove_file(path)?;
            continue;
        }
        // Do not chmod files: fixtures may hard-link them outside this tree.
        fs::set_permissions(
            &path,
            fs::Permissions::from_mode(meta.permissions().mode() | 0o700),
        )?;
        pending.push((path.clone(), true));
        // Close each directory before descending; depth consumes no handles
        // or call stack, including fixtures testing overdeep trees.
        for entry in fs::read_dir(path)? {
            pending.push((entry?.path(), false));
        }
    }
    Ok(())
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Some(path) = self.owned.take() {
            if let Err(e) = remove(&path) {
                eprintln!("cleanup {}: {e}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    #[test]
    fn drop_restores_permissions_without_following_links() {
        let outside = TempDir::new("td-temp-outside").unwrap();
        fs::write(outside.join("kept"), "kept").unwrap();
        let dir = TempDir::new("td-temp-locked").unwrap();
        let path = dir.path().to_path_buf();
        fs::create_dir_all(dir.join("locked/inner")).unwrap();
        fs::write(dir.join("locked/inner/file"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.join("link")).unwrap();
        for p in [dir.join("locked/inner"), dir.join("locked"), path.clone()] {
            fs::set_permissions(p, fs::Permissions::from_mode(0o000)).unwrap();
        }
        drop(dir);
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(outside.join("kept")).unwrap(), "kept");
    }
    #[test]
    fn hard_links_keep_their_modes_and_names_are_unique() {
        let outside = TempDir::new("td-temp-hardlink").unwrap();
        let first = TempDir::new("td-temp-hardlink").unwrap();
        let second = TempDir::new("td-temp-hardlink").unwrap();
        assert_ne!(first.path(), second.path());
        fs::write(outside.join("kept"), "kept").unwrap();
        fs::set_permissions(outside.join("kept"), fs::Permissions::from_mode(0o400)).unwrap();
        fs::hard_link(outside.join("kept"), first.join("linked")).unwrap();
        let path = first.path().to_path_buf();
        drop(first);
        assert!(!path.exists());
        assert_eq!(
            fs::metadata(outside.join("kept"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o400
        );
    }

    #[test]
    fn explicit_cleanup_removes_siblings_after_fixture_recreation() {
        let dir = TempDir::new("td-temp-recreate").unwrap();
        let parent = dir.path().parent().unwrap().to_path_buf();
        fs::remove_dir(dir.path()).unwrap();
        fs::create_dir(dir.path()).unwrap();
        fs::write(parent.join("d.commit.lock"), "held").unwrap();
        dir.cleanup().unwrap();
        assert!(!parent.exists());
    }

    #[test]
    fn unwinding_removes_the_fixture() {
        let dir = TempDir::new("td-temp-unwind").unwrap();
        let path = dir.path().to_path_buf();
        let result = std::panic::catch_unwind(move || {
            let _dir = dir;
            panic!("fixture failure");
        });
        assert!(result.is_err());
        assert!(!path.exists());
    }
}
