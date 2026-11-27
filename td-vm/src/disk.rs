//! Empty qcow2 v3 images for td's managed, single-file templates.
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

use crate::disk_lock;

const CLUSTER: u64 = 65536;
const NOFOLLOW_NONBLOCK: i32 = 0x20000 | 0x800;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn open(path: &Path, writable: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(writable)
        .custom_flags(NOFOLLOW_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("disk must be a regular file"));
    }
    Ok(file)
}

/// Keep the stopped image exclusive through the caller's filesystem operation.
/// Linux flock (td-vmm) and POSIX/OFD locks (QEMU) are independent.
pub fn lock_idle(path: &Path) -> io::Result<File> {
    let file = open(path, true)?;
    file.try_lock()?;
    disk_lock::idle(&file)?;
    Ok(file)
}

/// Copy one sector-aligned raw fixture into an unpublished instance.
pub fn copy_raw(source: &Path, dest: &Path) -> io::Result<()> {
    let mut file = open(source, false)?;
    file.try_lock_shared()?;
    disk_lock::template(&file)?;
    let size = file.metadata()?.len();
    if size == 0 || !size.is_multiple_of(512) {
        return Err(invalid("raw disk must contain whole sectors"));
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)?;
    if io::copy(&mut file, &mut output)? != size {
        return Err(invalid("raw image size changed during copy"));
    }
    output.sync_all()
}

/// A private staging directory owns this new image until creation succeeds.
pub fn create_blank(path: &Path, size: u64) -> io::Result<()> {
    create(path, size, None)
}

pub fn create_overlay(path: &Path, base: &Path) -> io::Result<()> {
    let file = open(base, false)?;
    file.try_lock_shared()?;
    disk_lock::template(&file)?;
    let mut header = [0u8; 104];
    file.read_exact_at(&mut header, 0)?;
    fn field<const N: usize>(header: &[u8], offset: usize) -> io::Result<[u8; N]> {
        header
            .get(offset..)
            .and_then(|rest| rest.get(..N))
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| invalid("short managed qcow2 header"))
    }
    let u32_at = |at| field(&header, at).map(u32::from_be_bytes);
    let u64_at = |at| field(&header, at).map(u64::from_be_bytes);
    if header.get(..4) != Some(b"QFI\xfb") {
        return Err(invalid("invalid qcow2 magic"));
    }
    // The import path converts sources to ordinary standalone images. Other
    // valid formats can still use the original host qemu-img implementation.
    if u32_at(4)? != 3 || u64_at(8)? != 0 || u64_at(72)? != 0 || u32_at(32)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "template requires qemu-img",
        ));
    }
    let size = u64_at(24)?;
    let bits = u32_at(20)?;
    if !(9..=21).contains(&bits) {
        return Err(invalid("invalid template cluster size"));
    }
    let cluster = 1u64 << bits;
    let length = file.metadata()?.len();
    let header_size = u64::from(u32_at(100)?);
    let check_table = |offset: u64, bytes: u64| {
        offset >= cluster
            && offset.is_multiple_of(cluster)
            && bytes != 0
            && offset.checked_add(bytes).is_some_and(|end| end <= length)
    };
    let l1 = u64::from(u32_at(36)?);
    let refcount_clusters = u64::from(u32_at(56)?);
    if u32_at(96)? > 6
        || !(104..=cluster.min(length)).contains(&header_size)
        || header_size % 8 != 0
        || l1 < size.div_ceil(cluster * (cluster / 8))
        || !check_table(u64_at(40)?, l1 * 8)
        || !check_table(u64_at(48)?, refcount_clusters * cluster)
    {
        return Err(invalid("invalid managed template tables"));
    }
    create(path, size, Some(base))
}

fn create(path: &Path, size: u64, base: Option<&Path>) -> io::Result<()> {
    let backing = base.map(|p| p.as_os_str().as_bytes()).unwrap_or_default();
    if size == 0
        || !size.is_multiple_of(512)
        || backing.len() > 1023
        || base.is_some_and(|p| !p.is_absolute())
    {
        return Err(invalid("invalid qcow2 capacity or backing path"));
    }
    let l1 = size.div_ceil(CLUSTER * (CLUSTER / 8));
    if l1 > (32 << 20) / 8 {
        return Err(invalid("qcow2 L1 table exceeds 32 MiB"));
    }
    let metadata_clusters = 3 + (l1 * 8).div_ceil(CLUSTER);
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    out.set_len(metadata_clusters * CLUSTER)?;
    out.write_all(b"QFI\xfb")?;
    out.write_all(&3u32.to_be_bytes())?;
    out.write_all(&(if base.is_some() { 128u64 } else { 0 }).to_be_bytes())?;
    out.write_all(&(backing.len() as u32).to_be_bytes())?;
    out.write_all(&16u32.to_be_bytes())?;
    out.write_all(&size.to_be_bytes())?;
    out.write_all(&0u32.to_be_bytes())?;
    out.write_all(&(l1 as u32).to_be_bytes())?;
    out.write_all(&(3 * CLUSTER).to_be_bytes())?;
    out.write_all(&CLUSTER.to_be_bytes())?;
    out.write_all(&1u32.to_be_bytes())?;
    // No snapshots or feature bits. Default refcounts are 16-bit.
    out.write_all_at(&4u32.to_be_bytes(), 96)?;
    out.write_all_at(&104u32.to_be_bytes(), 100)?;
    if base.is_some() {
        out.write_all_at(b"\xe2\x79\x2a\xca\0\0\0\x05qcow2", 104)?;
        out.write_all_at(backing, 128)?;
    }
    out.write_all_at(&(2 * CLUSTER).to_be_bytes(), CLUSTER)?;
    for i in 0..metadata_clusters {
        out.write_all_at(&1u16.to_be_bytes(), 2 * CLUSTER + 2 * i)?;
    }
    out.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_copy_refuses_an_ofd_writer_without_flock() -> io::Result<()> {
        let dir = td_fs::TempDir::new("vm-raw-lock")?;
        let source = dir.join("source");
        let dest = dir.join("copy");
        std::fs::write(&source, [7; 512])?;
        let writer = open(&source, true)?;
        disk_lock::idle(&writer)?;
        assert!(
            matches!(copy_raw(&source, &dest), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
        );
        assert!(!dest.exists());
        drop(writer);
        copy_raw(&source, &dest)?;
        assert_eq!(std::fs::read(dest)?, [7; 512]);
        Ok(())
    }
}
