//! A pinned physical USB FIDO attachment shared by td-authd and td-jail.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

type Attachment = (File, File, PathBuf);

fn text(args: std::fmt::Arguments<'_>) -> io::Result<String> {
    crate::allocation::format(args).map_err(|_| io::ErrorKind::OutOfMemory.into())
}

pub(crate) fn open(index: u8) -> io::Result<Option<Attachment>> {
    use crate::hid_descriptor::{descriptor, key_mode, MAX_DESCRIPTOR};
    use std::os::unix::fs::OpenOptionsExt;

    let name = text(format_args!("hidraw{index}"))?;
    let path = text(format_args!("/dev/{name}"))?;
    let Ok(expected) = fs::symlink_metadata(&path) else {
        return Ok(None);
    };
    let (major, minor) = crate::hid_descriptor::device_numbers(expected.rdev());
    if !expected.file_type().is_char_device()
        || expected.uid() != 0
        || expected.nlink() != 1
        || minor != u64::from(index)
        || !key_mode(expected.gid(), expected.mode())
    {
        return Ok(None);
    }

    let char_path = text(format_args!("/sys/dev/char/{major}:{minor}"))?;
    let Ok(syspath) = fs::canonicalize(&char_path) else {
        return Ok(None);
    };
    if !physical_leaf(&syspath, &name) {
        return Ok(None);
    }
    let Ok(sysfs) = fs::File::open(&syspath) else {
        return Ok(None);
    };
    let descriptor_path = text(format_args!(
        "/proc/self/fd/{}/device/report_descriptor",
        sysfs.as_raw_fd()
    ))?;
    let Ok(mut raw) = fs::File::open(descriptor_path) else {
        return Ok(None);
    };
    let mut bytes = [0; MAX_DESCRIPTOR + 1];
    let mut used = 0;
    while let Some(space) = bytes.get_mut(used..).filter(|left| !left.is_empty()) {
        match raw.read(space) {
            Ok(0) => break,
            Ok(size) => used += size,
            Err(_) => return Ok(None),
        }
    }
    if !bytes
        .get(..used)
        .is_some_and(|bytes| descriptor(bytes).is_ok())
    {
        return Ok(None);
    }

    // NOFOLLOW | NOCTTY | NONBLOCK. The real open pins the kernel minor
    // against reuse even after unplug, until the instance has ended.
    let Ok(node) = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(0o400000 | 0o400 | 0o4000)
        .open(&path)
    else {
        return Ok(None);
    };
    let actual = node.metadata()?;
    let pinned = sysfs.metadata()?;
    if (actual.dev(), actual.ino(), actual.rdev())
        != (expected.dev(), expected.ino(), expected.rdev())
        || actual.nlink() != 1
        || !fs::metadata(char_path)
            .is_ok_and(|now| (now.dev(), now.ino()) == (pinned.dev(), pinned.ino()))
    {
        return Ok(None);
    }
    Ok(Some((node, sysfs, syspath)))
}

pub(crate) fn physical_leaf(path: &Path, name: &str) -> bool {
    path.starts_with("/sys/devices")
        && !path.starts_with("/sys/devices/virtual")
        && path.file_name() == Some(OsStr::new(name))
        && path.parent().and_then(Path::file_name) == Some(OsStr::new("hidraw"))
        && path
            .ancestors()
            .nth(2)
            .and_then(Path::file_name)
            .is_some_and(|device| device.as_encoded_bytes().starts_with(b"0003:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_named_physical_usb_hidraw_leaf_is_eligible() {
        let physical = Path::new("/sys/devices/usb/0003:1234:4321.0001/hidraw/hidraw5");
        assert!(physical_leaf(physical, "hidraw5"));
        assert!(!physical_leaf(physical, "hidraw6"));
        for other in [
            "/sys/devices/virtual/misc/uhid/0003:1234:4321.0001/hidraw/hidraw5",
            "/sys/devices/i2c/0018:1234:4321.0001/hidraw/hidraw5",
            "/sys/devices/usb/0003:1234:4321.0001/input/hidraw5",
            "/sys/class/hidraw/hidraw5",
        ] {
            assert!(!physical_leaf(Path::new(other), "hidraw5"));
        }
    }
}
