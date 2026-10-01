//! A writable loop device over one byte range of a descriptor this process
//! already holds (DESIGN.md "Publishing through a loop over the claim").
//!
//! The descriptor is the destination's exclusive claim. The loop takes its
//! own reference to that same open file, so the claim lasts while the loop is
//! bound, and nothing is reopened by name. The loop clears itself when its
//! last opener closes; `LoopDevice::release` closes this process's opener and
//! confirms the kernel cleared it.

use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::loop_sys;
use crate::{device_numbers, invalid, paths};

/// The kernel's `LOOP_MAJOR`.
const LOOP_MAJOR: u64 = 7;
/// Binding is tried this many times, each with a fresh free index.
const ATTEMPTS: usize = 4;
/// How long a released loop may take to clear: polled every 10 ms.
const CLEAR_POLLS: usize = 100;

/// A bound loop device, held open read-only.
pub(crate) struct LoopDevice {
    device: File,
    minor: u64,
    path: PathBuf,
}

impl LoopDevice {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Close this process's opener and require the loop cleared, which it
    /// does only when nothing else holds it open or mounted.
    pub(crate) fn release(self) -> io::Result<()> {
        let Self {
            device,
            minor,
            path,
        } = self;
        drop(device);
        let bound = sysfs(minor).join("loop");
        for _ in 0..CLEAR_POLLS {
            if paths::metadata_if_present(&bound).is_none() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(invalid(format!(
            "{} is still bound after its last opener here closed",
            path.display()
        )))
    }
}

/// Bind a free loop device over `backing`'s bytes `offset..offset + len`,
/// with `block_size`-byte logical blocks, and check what the kernel made of
/// it before handing it out.
pub(crate) fn attach(
    backing: &File,
    offset: u64,
    len: u64,
    block_size: u64,
) -> io::Result<LoopDevice> {
    let blocks = u32::try_from(block_size)
        .map_err(|_| invalid(format!("block size {block_size} is out of range")))?;
    let identity = backing.metadata()?;
    let control_path = Path::new("/dev/loop-control");
    let control = paths::open_read_write(control_path)?;
    let mut busy = None;
    for _ in 0..ATTEMPTS {
        let index = loop_sys::free_loop(&control).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", control_path.display()))
        })?;
        let path = PathBuf::from(format!("/dev/loop{index}"));
        // Read-write, or the kernel binds it read-only. A device another
        // process bound and mounted since it was found free refuses this.
        let device = match paths::open_read_write(&path) {
            Ok(device) => device,
            Err(error) if error.kind() == io::ErrorKind::ResourceBusy => {
                busy = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let minor = loop_minor(&device, &path)?;
        match loop_sys::configure(&device, backing, offset, len, blocks) {
            Ok(()) => {
                // A refusal from here closes every opener here; the loop
                // clears unless the autoclear flag itself did not take,
                // which this check then reports.
                check_bound(minor, &identity, offset, len, block_size)
                    .map_err(|error| invalid(format!("{}: {error}", path.display())))?;
                // Held read-only from here: a kernel without
                // CONFIG_BLK_DEV_WRITE_MOUNTED refuses to mount a device
                // another descriptor holds open for writing. The reader
                // opens before the writer closes, so the loop keeps an
                // opener throughout.
                let held = paths::open_read(&path)?;
                if loop_minor(&held, &path)? != minor {
                    return Err(invalid(format!("{} is another device now", path.display())));
                }
                drop(device);
                return Ok(LoopDevice {
                    device: held,
                    minor,
                    path,
                });
            }
            // Another process bound it after it was found free.
            Err(error) if error.kind() == io::ErrorKind::ResourceBusy => busy = Some(error),
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("configure {}: {error}", path.display()),
                ))
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::ResourceBusy,
        format!(
            "no free loop device stayed free for {ATTEMPTS} attempts: {}",
            busy.map_or_else(String::new, |error| error.to_string())
        ),
    ))
}

fn sysfs(minor: u64) -> PathBuf {
    PathBuf::from(format!("/sys/dev/block/{LOOP_MAJOR}:{minor}"))
}

/// The opened node's minor number, refusing anything but a loop device.
fn loop_minor(device: &File, path: &Path) -> io::Result<u64> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = device.metadata()?;
    let (major, minor) = device_numbers(metadata.rdev());
    if !metadata.file_type().is_block_device() || major != LOOP_MAJOR {
        return Err(invalid(format!("{} is not a loop device", path.display())));
    }
    Ok(minor)
}

/// The kernel's own account of the bound device, asked by the number off the
/// opened node. A field the kernel took a different value from shows here;
/// the layout itself is pinned by `loop_sys.rs`'s byte test, since a
/// misplaced block size can be masked by the kernel's default.
fn check_bound(
    minor: u64,
    backing: &Metadata,
    offset: u64,
    len: u64,
    block_size: u64,
) -> io::Result<()> {
    let base = sysfs(minor);
    let sectors = len / 512;
    for (attribute, expected) in [
        ("loop/offset", offset),
        ("loop/sizelimit", len),
        ("loop/autoclear", 1),
        ("loop/partscan", 0),
        ("loop/dio", 0),
        ("ro", 0),
        ("size", sectors),
        ("queue/logical_block_size", block_size),
    ] {
        let path = base.join(attribute);
        let text = paths::read_to_string(&path)?;
        if text.trim().parse::<u64>().ok() != Some(expected) {
            return Err(invalid(format!(
                "{} reads {:?}, not {expected}",
                path.display(),
                text.trim()
            )));
        }
    }
    let named = base.join("loop/backing_file");
    let text = paths::read_to_string(&named)?;
    let file = paths::metadata(Path::new(text.trim_end_matches('\n')))?;
    if !same_file(backing, &file) {
        return Err(invalid(format!(
            "{} names {:?}, which is not the file bound",
            named.display(),
            text.trim_end()
        )));
    }
    Ok(())
}

/// Whether two stats name one file: a device by its number, anything else by
/// its inode.
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    if a.file_type().is_block_device() || b.file_type().is_block_device() {
        a.file_type().is_block_device() && b.file_type().is_block_device() && a.rdev() == b.rdev()
    } else {
        (a.dev(), a.ino()) == (b.dev(), b.ino())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_loop_device_is_bound() {
        let path = Path::new("/dev/null");
        let file = std::fs::File::open(path).unwrap();
        let error = loop_minor(&file, path).unwrap_err();
        assert!(error.to_string().contains("not a loop device"), "{error}");
    }

    #[test]
    fn a_backing_file_is_known_by_its_inode_and_a_device_by_its_number() {
        let null = std::fs::metadata("/dev/null").unwrap();
        let zero = std::fs::metadata("/dev/zero").unwrap();
        let here = std::fs::metadata(file!()).unwrap();
        let again = std::fs::metadata(file!()).unwrap();
        let other = std::fs::metadata("Cargo.toml").unwrap();
        assert!(same_file(&here, &again));
        assert!(!same_file(&here, &other));
        // Character devices are files to this rule; only block devices
        // compare by number.
        assert!(same_file(&null, &null));
        assert!(!same_file(&null, &zero));
    }

    #[test]
    fn an_unrepresentable_block_size_refuses_before_any_device() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let error = attach(&file, 0, 512, u64::from(u32::MAX) + 1)
            .err()
            .unwrap();
        assert!(error.to_string().contains("out of range"), "{error}");
    }
}
