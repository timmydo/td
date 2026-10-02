//! Deleting what the window names. A target carries what the scan saw of
//! it: its device and inode, whether it is a directory, and for anything
//! else its length and modification time. A path that disagrees on any of
//! them now is refused, so a file replaced since the scan, even under a
//! reused inode number, is not the one deleted unless it also has the
//! same length and time. A target on another device than the directory
//! holding it (a mount, a btrfs subvolume) is refused. A directory is
//! removed depth first without following a symbolic link and without
//! entering another file system: a mount point at or beneath the target,
//! or an entry on another device, stops the deletion before anything
//! beneath it is touched.

use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::tree::Identity;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    pub path: PathBuf,
    pub identity: Identity,
    pub directory: bool,
    /// The length and modification time the scan saw; not compared for a
    /// directory, whose time moves with its entries.
    pub length: u64,
    pub mtime: i64,
}

/// Why the entry at a target's path is not the one scanned, if it is not.
pub fn mismatch(target: &Target, meta: &Metadata) -> Option<&'static str> {
    let identity = Identity {
        device: meta.dev(),
        inode: meta.ino(),
    };
    if identity != target.identity {
        return Some("is another file now");
    }
    if meta.is_dir() != target.directory {
        return Some("changed type");
    }
    if !target.directory && (meta.len() != target.length || meta.mtime() != target.mtime) {
        return Some("changed");
    }
    None
}

/// Whether an entry lies on the same device as the directory holding it.
pub fn same_device(entry: u64, parent: u64) -> bool {
    entry == parent
}

/// The mount points `/proc/self/mountinfo` lists: its fifth field, with
/// the kernel's octal escapes for space, tab, newline and backslash
/// undone. Bytes, not text: a mount point need not be UTF-8. A line it
/// cannot read is skipped.
pub fn mount_points(mountinfo: &[u8]) -> Vec<PathBuf> {
    mountinfo
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.split(|byte| *byte == b' ').nth(4))
        .map(|field| PathBuf::from(OsString::from_vec(unescape(field))))
        .collect()
}

fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut index = 0;
    while let Some(byte) = field.get(index).copied() {
        let octal = field
            .get(index + 1..index + 4)
            .filter(|digits| digits.iter().all(|d| (b'0'..=b'7').contains(d)))
            .map(|digits| {
                digits
                    .iter()
                    .fold(0u32, |value, d| value * 8 + u32::from(d - b'0'))
            })
            .and_then(|value| u8::try_from(value).ok());
        match octal {
            Some(value) if byte == b'\\' => {
                out.push(value);
                index += 4;
            }
            _ => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

/// Whether a mount point lies at `path` or beneath it.
pub fn holds_mount(path: &Path, mounts: &[PathBuf]) -> bool {
    mounts.iter().any(|mount| mount.starts_with(path))
}

fn refuse(message: String) -> io::Error {
    io::Error::other(message)
}

/// Deletes the target. A directory is checked against the mount table
/// first, which must be readable.
pub fn delete(target: &Target) -> io::Result<()> {
    let meta = fs::symlink_metadata(&target.path)?;
    if let Some(why) = mismatch(target, &meta) {
        return Err(refuse(format!(
            "{} {why} since it was scanned; refresh first",
            target.path.display()
        )));
    }
    let parent = target
        .path
        .parent()
        .ok_or_else(|| refuse(format!("{} has no parent", target.path.display())))?;
    if !same_device(meta.dev(), fs::symlink_metadata(parent)?.dev()) {
        return Err(refuse(format!(
            "{} is on another file system than its directory",
            target.path.display()
        )));
    }
    if !meta.is_dir() {
        return fs::remove_file(&target.path);
    }
    let mountinfo = fs::read("/proc/self/mountinfo")
        .map_err(|error| refuse(format!("cannot read the mount table: {error}")))?;
    let canonical = fs::canonicalize(&target.path)?;
    if holds_mount(&canonical, &mount_points(&mountinfo)) {
        return Err(refuse(format!(
            "{} is or holds a mount point",
            target.path.display()
        )));
    }
    remove_tree(&target.path, target.identity)
}

/// What another process removing an entry first looks like; the walk
/// goes on without it.
fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

/// Removes a directory depth first, every entry read without following
/// links. Each directory is checked again just before it is listed: it
/// must still be a directory, not a link, with the device and inode it
/// was listed with, so one swapped for a link since is not followed. An
/// entry on another device ends the walk with an error.
fn remove_tree(top: &Path, identity: Identity) -> io::Result<()> {
    let device = identity.device;
    let mut stack = vec![(top.to_path_buf(), identity, false)];
    while let Some((dir, expected, listed)) = stack.pop() {
        if listed {
            match fs::remove_dir(&dir) {
                Err(error) if !gone(&error) => return Err(error),
                _ => continue,
            }
        }
        // A directory below the top another process removed first is done.
        let below = dir != top;
        let now = match fs::symlink_metadata(&dir) {
            Ok(now) => now,
            Err(error) if below && gone(&error) => continue,
            Err(error) => return Err(error),
        };
        let actual = Identity {
            device: now.dev(),
            inode: now.ino(),
        };
        if !now.is_dir() || actual != expected {
            return Err(refuse(format!(
                "{} changed while it was being deleted",
                dir.display()
            )));
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if below && gone(&error) => continue,
            Err(error) => return Err(error),
        };
        stack.push((dir.clone(), expected, true));
        for entry in entries {
            let entry = entry?;
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(error) if gone(&error) => continue,
                Err(error) => return Err(error),
            };
            if !same_device(meta.dev(), device) {
                return Err(refuse(format!(
                    "{} is on another file system",
                    entry.path().display()
                )));
            }
            if meta.is_dir() {
                let child = Identity {
                    device: meta.dev(),
                    inode: meta.ino(),
                };
                stack.push((entry.path(), child, false));
            } else {
                match fs::remove_file(entry.path()) {
                    Err(error) if !gone(&error) => return Err(error),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_points_are_unescaped() {
        let table = b"22 1 0:21 / / rw - ext4 /dev/root rw\n\
                     30 22 0:30 / /mnt/my\\040disk rw - ext4 /dev/sdb rw\n\
                     31 22 0:31 / /a\\134b rw - tmpfs t rw\n\
                     32 22 0:32 / /l\xe9 rw - vfat u rw\n\
                     33 22 0:33 / /x\\9 rw - tmpfs t rw\n\
                     short line\n";
        assert_eq!(
            mount_points(table),
            vec![
                PathBuf::from("/"),
                PathBuf::from("/mnt/my disk"),
                PathBuf::from("/a\\b"),
                PathBuf::from(OsString::from_vec(b"/l\xe9".to_vec())),
                PathBuf::from("/x\\9"),
            ]
        );
    }

    #[test]
    fn a_mount_beneath_holds() {
        let mounts = vec![PathBuf::from("/"), PathBuf::from("/home/u/mnt")];
        assert!(holds_mount(Path::new("/home/u"), &mounts));
        assert!(holds_mount(Path::new("/home/u/mnt"), &mounts));
        assert!(!holds_mount(Path::new("/home/u/mn"), &mounts));
        assert!(!holds_mount(Path::new("/home/u/mnt/x"), &mounts));
    }

    #[test]
    fn devices() {
        assert!(same_device(5, 5));
        assert!(!same_device(5, 6));
    }
}
