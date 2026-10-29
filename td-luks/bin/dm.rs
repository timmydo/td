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

pub fn activate(
    file: &mut File,
    name: &str,
    uuid: &str,
    key: &[u8; 64],
    read_only: bool,
) -> io::Result<()> {
    let path = entry(name)?;
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
    let _mapping = crate::lock::mapping(name)?;
    let control = Control::open()?;
    if control.status(name)?.is_some() {
        return Err(invalid("mapping already exists"));
    }
    removable(&path)?;
    let uuid = format!("CRYPT-LUKS2-{}-{name}", uuid.replace('-', ""));
    let status = control.create_crypt(
        name,
        &uuid,
        length,
        device_number(meta.rdev()),
        key,
        read_only,
    )?;
    publish(&control, &status, name)
}

pub(crate) fn publish(control: &Control, status: &Status, name: &str) -> io::Result<()> {
    let path = entry(name)?;
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
    let control = Control::open()?;
    let before = control
        .status(name)?
        .ok_or_else(|| invalid("mapping is absent"))?;
    let compact = before
        .uuid
        .strip_prefix("CRYPT-LUKS2-")
        .and_then(|s| s.get(..32))
        .filter(|s| {
            s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or_else(|| invalid("mapping has no canonical LUKS2 UUID"))?;
    let part = |range: std::ops::Range<usize>| {
        compact
            .get(range)
            .ok_or_else(|| invalid("short mapping UUID"))
    };
    let volume_uuid = format!(
        "{}-{}-{}-{}-{}",
        part(0..8)?,
        part(8..12)?,
        part(12..16)?,
        part(16..20)?,
        part(20..32)?
    );
    if before.uuid != crate::reencrypt_stack::uuid("LUKS2", &volume_uuid, name)? {
        return Err(invalid("mapping UUID/name mismatch"));
    }
    let _reencryption = crate::lock::reencryption(&volume_uuid)?;
    let _mapping = crate::lock::mapping(name)?;
    let status = control
        .status(name)?
        .ok_or_else(|| invalid("mapping is absent"))?;
    if status.device != before.device || status.uuid != before.uuid {
        return Err(invalid("mapping changed while locking"));
    }
    let helpers = owned_helpers(&control, name, &volume_uuid)?;
    let mut helper_locks = Vec::new();
    for helper in &helpers {
        helper_locks.push(crate::lock::mapping(&helper.name)?);
    }

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
    for helper in helpers {
        let current = control
            .status(&helper.name)?
            .ok_or_else(|| invalid("helper disappeared while closing"))?;
        if current.uuid != helper.uuid || current.device != helper.device {
            return Err(invalid("helper changed while closing"));
        }
        control.remove(&helper.name)?;
        self::remove_entry(&helper.name, helper.device)?;
    }
    Ok(())
}

pub(crate) fn remove_entry(name: &str, device: u64) -> io::Result<()> {
    let path = entry(name)?;
    if removable(&path)? {
        match fs::metadata(&path) {
            Ok(meta) if meta.file_type().is_block_device() && meta.rdev() == device => {
                fs::remove_file(path)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::remove_file(path)?;
            }
            _ => return Err(invalid("mapper entry changed identity")),
        }
    }
    Ok(())
}

fn owned_helpers(control: &Control, name: &str, uuid: &str) -> io::Result<Vec<Status>> {
    let (overlay, hotzone) = crate::reencrypt_stack::names(name)?;
    let wanted = [overlay, hotzone];
    let mut pending = vec![name.to_string()];
    let mut found = Vec::<Status>::new();
    let mut backing = Vec::<u64>::new();
    while let Some(parent) = pending.pop() {
        for dev in control.dependencies(&parent)? {
            let (major, minor) = device_number(dev);
            let path = format!("/sys/dev/block/{major}:{minor}/dm/name");
            let child = match fs::read_to_string(path) {
                Ok(text) => text.trim().to_owned(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    backing.push(dev);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if !wanted.contains(&child) {
                backing.push(dev);
                continue;
            }
            if found.iter().any(|s| s.name == child) {
                continue;
            }
            let status = control
                .status(&child)?
                .ok_or_else(|| invalid("dependency disappeared"))?;
            if !crate::reencrypt_stack::uuid("SUBDEV", uuid, &child)
                .is_ok_and(|expected| status.uuid == expected)
            {
                continue;
            }
            if found.len() >= 2 {
                return Err(invalid("too many reencryption helpers"));
            }
            pending.push(child);
            found.push(status);
        }
    }
    // A crash can leave helpers that no active top table references yet,
    // or no longer references. Admit them bottom-up against known backing.
    for name in wanted.iter().rev() {
        if found.iter().any(|s| &s.name == name) {
            continue;
        }
        let Some(status) = control.status(name)? else {
            continue;
        };
        if !crate::reencrypt_stack::uuid("SUBDEV", uuid, name)
            .is_ok_and(|expected| status.uuid == expected)
        {
            continue;
        }
        let deps = control.dependencies(name)?;
        if !deps.is_empty()
            && deps
                .iter()
                .all(|dev| backing.contains(dev) || found.iter().any(|s| s.device == *dev))
        {
            found.push(status);
        }
    }
    found.sort_by_key(|status| usize::from(wanted.first() != Some(&status.name)));
    Ok(found)
}
