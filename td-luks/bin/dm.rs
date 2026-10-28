//! Mapping lifetime and /dev/mapper publication, outside the syscall boundary.
use crate::dm_sys::{device_number, Control, Status};
use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom};
use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn entry(name: &str) -> io::Result<PathBuf> {
    if !crate::dm_sys::valid_name(name) {
        return Err(invalid("invalid mapping name"));
    }
    Ok(Path::new("/dev/mapper").join(name))
}
fn removable(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || meta.file_type().is_block_device() => Ok(true),
        Ok(_) => Err(invalid(
            "mapper entry is neither a symlink nor a block device",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn activate(file: &mut File, name: &str, uuid: &str, key: &[u8; 64]) -> io::Result<()> {
    let path = entry(name)?;
    let _mapping = crate::lock::mapping(name)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_block_device() {
        return Err(invalid("activation requires a block device"));
    }
    let length = file
        .seek(SeekFrom::End(0))?
        .checked_sub(td_luks::write::DATA_OFFSET)
        .filter(|n| *n > 0 && n.is_multiple_of(td_luks::write::SECTOR_SIZE))
        .ok_or_else(|| invalid("invalid encrypted data size"))?
        / 512;
    let control = Control::open()?;
    if control.status(name)?.is_some() {
        return Err(invalid("mapping already exists"));
    }
    removable(&path)?;
    let uuid = format!("CRYPT-LUKS2-{}-{name}", uuid.replace('-', ""));
    let status = control.create_crypt(name, &uuid, length, device_number(meta.rdev()), key)?;
    let (_, minor) = device_number(status.device);
    let target = format!("../dm-{minor}");
    let publish = (|| {
        let node = fs::metadata(format!("/dev/dm-{minor}"))?;
        if !node.file_type().is_block_device() || node.rdev() != status.device {
            return Err(invalid("device-mapper node has unexpected identity"));
        }
        // CREATE claims the kernel name before a stale entry can be removed.
        // A competing opener that lost CREATE must leave the winner's entry.
        if removable(&path)? {
            fs::remove_file(&path)?;
        }
        symlink(&target, &path)
    })();
    if let Err(error) = publish {
        control
            .remove(name)
            .map_err(|_| invalid("publication failed and mapping cleanup failed"))?;
        return Err(error);
    }
    Ok(())
}

pub fn status(name: &str) -> io::Result<Option<Status>> {
    entry(name)?;
    Control::open()?.status(name)
}

pub fn close(name: &str) -> io::Result<()> {
    let path = entry(name)?;
    let _mapping = crate::lock::mapping(name)?;
    let control = Control::open()?;
    let status = control
        .status(name)?
        .ok_or_else(|| invalid("mapping is absent"))?;
    if !status.uuid.starts_with("CRYPT-LUKS2-") {
        return Err(invalid("mapping is not LUKS2"));
    }
    // Refuse an unrelated filesystem entry before changing the live mapping.
    let remove_entry = removable(&path)?;
    if remove_entry {
        match fs::metadata(&path) {
            Ok(meta) if meta.file_type().is_block_device() && meta.rdev() == status.device => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return Err(invalid("mapper entry names another device")),
        }
    }
    control.remove(name)?;
    if remove_entry {
        fs::remove_file(path)?;
    }
    Ok(())
}
