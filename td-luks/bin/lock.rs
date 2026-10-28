//! cryptsetup 2.8.8's device locks (`lib/utils_device_locking.c`), so that
//! td-luks and stock cryptsetup exclude each other (DESIGN.md "Locking").
//!
//! A block device is locked through the resource file
//! `/run/cryptsetup/L_<major>:<minor>`: created `O_NOFOLLOW` with mode
//! 0644 under a lock directory created 0700, `flock`ed, then checked to
//! be still the file at that path, since a releasing holder unlinks it.
//! A regular file is `flock`ed itself, with no resource file and no
//! check. Shared for a read, exclusive for a mutation, both blocking.
//!
//! cryptsetup opens the resource relative to the lock directory's
//! descriptor; std has no `openat`, so this opens it by path after
//! checking the directory is no symlink. Only root can create that
//! directory or swap it between the two, and root needs no race to
//! defeat a lock.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// cryptsetup's compiled-in lock directory, which td's recipe keeps.
pub const LOCK_DIR: &str = "/run/cryptsetup";
const LOCK_DIR_MODE: u32 = 0o700;
const RESOURCE_MODE: u32 = 0o644;
// Linux x86-64 open(2) flags std does not name.
const O_NOFOLLOW: i32 = 0o400_000;
const O_DIRECTORY: i32 = 0o200_000;
const O_NONBLOCK: i32 = 0o4_000;

/// How a lock is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `LOCK_SH`: a header read.
    Read,
    /// `LOCK_EX`: a header mutation.
    Write,
}

/// A held lock; dropping it releases it as cryptsetup does.
#[derive(Debug)]
pub struct Lock {
    file: File,
    /// The resource path of a block device's lock, which a release may
    /// unlink; `None` for a regular file, locked as itself.
    resource: Option<PathBuf>,
}

/// Open once, then lock the exact device used by the operation.
pub fn open(device: &Path, mode: Mode, exclusive: bool) -> io::Result<(File, Lock)> {
    open_in(Path::new(LOCK_DIR), device, mode, exclusive)
}

fn open_in(dir: &Path, device: &Path, mode: Mode, exclusive: bool) -> io::Result<(File, Lock)> {
    // O_EXCL without O_CREAT excludes a claimed block device and has no
    // effect on regular files. Prefer writable reads for NFSv4 flock.
    const O_EXCL: i32 = 0o200;
    let flags = O_NONBLOCK | if exclusive { O_EXCL } else { 0 };
    let held = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(flags)
        .open(device)
    {
        Ok(file) => file,
        Err(_) if mode == Mode::Read => OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(device)?,
        Err(error) => return Err(error),
    };
    let meta = held.metadata()?;
    let kind = meta.file_type();
    let lock = if kind.is_block_device() {
        resource_in(dir, meta.rdev(), mode)?
    } else if kind.is_file() {
        take(&held, mode)?;
        Lock {
            file: held.try_clone()?,
            resource: None,
        }
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is neither a block device nor a file", device.display()),
        ));
    };
    Ok((held, lock))
}

#[cfg(test)]
fn device_in(dir: &Path, device: &Path, mode: Mode) -> io::Result<Lock> {
    open_in(dir, device, mode, false).map(|(_, lock)| lock)
}

/// Lock the resource file for the block device numbered `rdev` under
/// `dir`, retrying while a releasing holder unlinks it underneath.
pub fn resource_in(dir: &Path, rdev: u64, mode: Mode) -> io::Result<Lock> {
    resource_at(lock_dir(dir)?.join(resource_name(rdev)), mode)
}

/// Serialize td-luks's mapping-name lifecycle independently of volume identity.
pub fn mapping(name: &str) -> io::Result<Lock> {
    mapping_in(Path::new(LOCK_DIR), name)
}

fn mapping_in(dir: &Path, name: &str) -> io::Result<Lock> {
    if !crate::dm_sys::valid_name(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid mapping name",
        ));
    }
    resource_at(
        lock_dir(dir)?.join(format!("LN_td-luks-mapping-{name}")),
        Mode::Write,
    )
}

fn resource_at(path: PathBuf, mode: Mode) -> io::Result<Lock> {
    loop {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(RESOURCE_MODE)
            .custom_flags(O_NOFOLLOW)
            .open(&path)?;
        take(&file, mode)?;
        let lock = Lock {
            file,
            resource: Some(path.clone()),
        };
        if lock.still_named()? {
            return Ok(lock);
        }
        // Another holder unlinked the file before this one locked it:
        // drop this lock, whose release finds nothing to unlink, and
        // take the file now at the path.
        drop(lock);
    }
}

/// `L_<major>:<minor>`, glibc's split of a Linux device number.
pub fn resource_name(rdev: u64) -> String {
    let major = ((rdev >> 8) & 0x0000_0fff) | ((rdev >> 32) & 0xffff_f000);
    let minor = (rdev & 0x0000_00ff) | ((rdev >> 12) & 0xffff_ff00);
    format!("L_{major}:{minor}")
}

/// The lock directory, created 0700 when missing, refused when it is a
/// symlink or no directory, as cryptsetup's `open_lock_dir` refuses it.
fn lock_dir(dir: &Path) -> io::Result<PathBuf> {
    let open = || {
        OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_DIRECTORY)
            .open(dir)
    };
    match open() {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match DirBuilder::new().mode(LOCK_DIR_MODE).create(dir) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            open().map_err(|error| unusable(dir, error))?;
        }
        Err(error) => return Err(unusable(dir, error)),
    }
    Ok(dir.to_path_buf())
}

fn unusable(dir: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("the locking path {} is unusable: {error}", dir.display()),
    )
}

fn take(file: &File, mode: Mode) -> io::Result<()> {
    match mode {
        Mode::Read => file.lock_shared(),
        Mode::Write => file.lock(),
    }
}

impl Lock {
    /// Whether the resource path still names the locked file;
    /// `verify_lock_handle`. A regular file is never unlinked.
    fn still_named(&self) -> io::Result<bool> {
        let Some(path) = &self.resource else {
            return Ok(true);
        };
        let held = self.file.metadata()?;
        Ok(match fs::metadata(path) {
            Ok(named) => same_inode(&held, &named),
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        })
    }
}

fn same_inode(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

impl Drop for Lock {
    /// `release_lock_handle`: a block device's resource file is unlinked
    /// when no one else holds or waits on it, which only a non-blocking
    /// exclusive lock can tell, and only while the path still names it.
    fn drop(&mut self) {
        let Some(path) = &self.resource else {
            return;
        };
        if self.file.try_lock().is_ok() && matches!(self.still_named(), Ok(true)) {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use td_fs::TempDir;

    // makedev(253, 7): a device-mapper minor.
    const RDEV: u64 = (253 << 8) | 7;

    fn other_handle(path: &Path) -> File {
        File::open(path).unwrap()
    }

    #[test]
    fn mapping_lifecycles_exclude_by_name_and_release_the_resource() {
        let temp = TempDir::new("td-luks-mapping-lock").unwrap();
        let dir = temp.path().join("cryptsetup");
        let path = dir.join("LN_td-luks-mapping-volume");
        let first = mapping_in(&dir, "volume").unwrap();
        assert!(other_handle(&path).try_lock().is_err());
        let other = mapping_in(&dir, "other").unwrap();
        let (sent, got) = mpsc::channel();
        let waiter_dir = dir.clone();
        let waiter = thread::spawn(move || {
            let lock = mapping_in(&waiter_dir, "volume").unwrap();
            sent.send(()).unwrap();
            lock
        });
        assert!(got.recv_timeout(Duration::from_millis(100)).is_err());
        drop(first);
        got.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = waiter.join().unwrap();
        assert!(other_handle(&path).try_lock().is_err());
        drop(second);
        drop(other);
        assert!(!path.exists());
        assert!(mapping_in(&dir, "../escape").is_err());
    }

    #[test]
    fn device_numbers_split_as_glibc_splits_them() {
        assert_eq!(resource_name(RDEV), "L_253:7");
        // makedev(259, 300000): both halves past their legacy bits.
        let major = 259u64;
        let minor = 300_000u64;
        let rdev = ((major & 0xfff) << 8)
            | ((major & !0xfff) << 32)
            | (minor & 0xff)
            | ((minor & !0xff) << 12);
        assert_eq!(resource_name(rdev), "L_259:300000");
    }

    #[test]
    fn the_lock_directory_is_created_private_and_a_symlink_refused() {
        let temp = TempDir::new("td-luks-lockdir").unwrap();
        let dir = temp.path().join("cryptsetup");
        let lock = resource_in(&dir, RDEV, Mode::Write).unwrap();
        let meta = fs::symlink_metadata(&dir).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.mode() & 0o777, LOCK_DIR_MODE);
        let file = fs::symlink_metadata(dir.join("L_253:7")).unwrap();
        assert_eq!(file.mode() & 0o777 & !RESOURCE_MODE, 0);
        assert_eq!(file.mode() & 0o600, 0o600);
        drop(lock);

        let elsewhere = temp.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        let error = resource_in(&link, RDEV, Mode::Write).unwrap_err();
        assert!(error.to_string().contains("unusable"), "{error}");
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[test]
    fn a_symlinked_resource_file_is_refused() {
        let temp = TempDir::new("td-luks-lockfile").unwrap();
        let dir = temp.path().join("cryptsetup");
        fs::create_dir(&dir).unwrap();
        let target = temp.path().join("target");
        std::os::unix::fs::symlink(&target, dir.join("L_253:7")).unwrap();
        assert!(resource_in(&dir, RDEV, Mode::Read).is_err());
        assert!(!target.exists());
    }

    #[test]
    fn writers_exclude_and_readers_share_a_resource() {
        let temp = TempDir::new("td-luks-share").unwrap();
        let dir = temp.path().join("cryptsetup");
        let path = dir.join("L_253:7");

        let first = resource_in(&dir, RDEV, Mode::Read).unwrap();
        let second = resource_in(&dir, RDEV, Mode::Read).unwrap();
        assert!(other_handle(&path).try_lock().is_err());
        drop(first);
        drop(second);

        let writer = resource_in(&dir, RDEV, Mode::Write).unwrap();
        assert!(other_handle(&path).try_lock_shared().is_err());
        drop(writer);
        // The last holder unlinked the file it alone held.
        assert!(!path.exists());
    }

    #[test]
    fn a_holder_still_waited_on_keeps_the_file() {
        let temp = TempDir::new("td-luks-keep").unwrap();
        let dir = temp.path().join("cryptsetup");
        let path = dir.join("L_253:7");
        let first = resource_in(&dir, RDEV, Mode::Read).unwrap();
        let second = resource_in(&dir, RDEV, Mode::Read).unwrap();
        // A release while another reader holds the file cannot take it
        // exclusively, so it leaves it for that reader.
        drop(first);
        assert!(path.exists());
        drop(second);
        assert!(!path.exists());
    }

    /// The race `verify_lock_handle` closes: a waiter that locks a file
    /// its releasing holder has just unlinked retries on the new one.
    #[test]
    fn a_waiter_on_an_unlinked_file_retries_on_the_new_one() {
        let temp = TempDir::new("td-luks-race").unwrap();
        let dir = temp.path().join("cryptsetup");
        let path = dir.join("L_253:7");
        let holder = resource_in(&dir, RDEV, Mode::Write).unwrap();
        let first_inode = fs::metadata(&path).unwrap().ino();
        // An unlocked handle keeps the first inode allocated, so the new
        // file cannot reuse its number.
        let _keep = other_handle(&path);

        let (sent, got) = mpsc::channel();
        let waiter_dir = dir.clone();
        let waiter = thread::spawn(move || {
            let lock = resource_in(&waiter_dir, RDEV, Mode::Write).unwrap();
            sent.send(()).unwrap();
            lock
        });
        // The waiter has opened the first file and blocks in flock.
        thread::sleep(Duration::from_millis(200));
        assert!(got.try_recv().is_err());
        // The release unlinks the first file, then closes it, which wakes
        // the waiter on an inode no longer at the path.
        drop(holder);
        let lock = waiter.join().unwrap();
        let named = fs::metadata(&path).unwrap();
        assert_ne!(named.ino(), first_inode);
        assert!(same_inode(&lock.file.metadata().unwrap(), &named));
        assert!(other_handle(&path).try_lock().is_err());
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn a_regular_file_is_locked_as_itself() {
        let temp = TempDir::new("td-luks-file").unwrap();
        let dir = temp.path().join("cryptsetup");
        let device = temp.path().join("disk");
        fs::write(&device, b"disk").unwrap();
        let lock = device_in(&dir, &device, Mode::Write).unwrap();
        assert!(other_handle(&device).try_lock_shared().is_err());
        // No lock directory or resource file for a regular file.
        assert!(!dir.exists());
        drop(lock);
        assert!(other_handle(&device).try_lock().is_ok());
        assert!(device.exists());

        let readers = (
            device_in(&dir, &device, Mode::Read).unwrap(),
            device_in(&dir, &device, Mode::Read).unwrap(),
        );
        assert!(other_handle(&device).try_lock().is_err());
        drop(readers);
    }

    #[test]
    fn neither_device_nor_file_is_refused() {
        let temp = TempDir::new("td-luks-kind").unwrap();
        let error = device_in(temp.path(), temp.path(), Mode::Read).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
