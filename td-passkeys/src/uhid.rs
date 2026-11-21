//! Linux UHID for the fixed FIDO application collection. The kernel
//! destroys the device when its last UHID descriptor closes.
use crate::{hid::Report, Error, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use td_primitives::random;

const DESC: &[u8] = &[
    0x06, 0xd0, 0xf1, 0x09, 1, 0xa1, 1, 0x09, 0x20, 0x15, 0, 0x26, 0xff, 0, 0x75, 8, 0x95, 64,
    0x81, 2, 0x09, 0x21, 0x95, 64, 0x91, 2, 0xc0,
];
const NAME: &[u8] = b"td website authenticator";
const PHYS: &[u8] = b"td/firefox-passkeys";
const FLAGS: i32 = 0o400000 | 0o4000 | 0o400;
const DESC_OFFSET: usize = 280;
const OUTPUT_SIZE: usize = 4100;
const SELECTOR: &str = "/dev/td-firefox-passkeys";

pub struct Device {
    file: File,
    identity: String,
    node: Option<File>,
}
fn put(buf: &mut [u8], at: usize, data: &[u8]) -> Result<()> {
    buf.get_mut(at..)
        .and_then(|remaining| remaining.get_mut(..data.len()))
        .ok_or(Error::Invalid)?
        .copy_from_slice(data);
    Ok(())
}
fn write_event(file: &mut File, bytes: &[u8]) -> Result<()> {
    loop {
        match file.write(bytes) {
            Ok(n) if n == bytes.len() => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
            Ok(_) => return Err(Error::Invalid),
        }
    }
}
impl Device {
    pub fn create() -> Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FLAGS)
            .open("/dev/uhid")?;
        let m = file.metadata()?;
        if !m.file_type().is_char_device()
            || (m.rdev(), m.uid(), m.gid(), m.mode() & 0o7777) != (0x0aef, 0, 0, 0o600)
        {
            return Err(Error::Refused);
        }
        // Only one td session may publish this selector at once.
        file.try_lock().map_err(|_| Error::Locked)?;
        let mut nonce = [0; 16];
        random::fill(&mut nonce)?;
        let identity = td_alloc::format(format_args!("td-{:032x}", u128::from_ne_bytes(nonce)))
            .map_err(|_| Error::Capacity)?;
        let mut event = [0; DESC_OFFSET + 4096];
        put(&mut event, 0, &11u32.to_ne_bytes())?;
        put(&mut event, 4, NAME)?;
        put(&mut event, 132, PHYS)?;
        put(&mut event, 196, identity.as_bytes())?;
        put(&mut event, 260, &(DESC.len() as u16).to_ne_bytes())?;
        put(&mut event, 262, &3u16.to_ne_bytes())?;
        put(&mut event, 264, &0x1209u32.to_ne_bytes())?;
        put(&mut event, 268, &0x7464u32.to_ne_bytes())?;
        put(&mut event, DESC_OFFSET, DESC)?;
        write_event(&mut file, &event)?;
        Ok(Self {
            file,
            identity,
            node: None,
        })
    }
    /// Bind only this open UHID device's kernel-created node. Called by
    /// the root owner after obtaining the Firefox workload's current gid.
    pub fn publish(&mut self, group: u32) -> Result<bool> {
        if !(65536..98304).contains(&group) {
            return Err(Error::Refused);
        }
        if let Some(node) = &self.node {
            let m = node.metadata()?;
            let selected = fs::metadata(SELECTOR)?;
            return if (m.gid(), m.mode() & 0o7777, m.nlink()) == (group, 0o660, 1)
                && (m.dev(), m.ino(), m.rdev()) == (selected.dev(), selected.ino(), selected.rdev())
            {
                Ok(true)
            } else {
                Err(Error::Stale)
            };
        }
        for index in 0..=u8::MAX {
            let name =
                td_alloc::format(format_args!("hidraw{index}")).map_err(|_| Error::Capacity)?;
            let path =
                td_alloc::format(format_args!("/dev/{name}")).map_err(|_| Error::Capacity)?;
            let meta = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if !meta.file_type().is_char_device()
                || (meta.uid(), meta.gid(), meta.mode() & 0o7777, meta.nlink()) != (0, 0, 0o600, 1)
            {
                continue;
            }
            let dev = meta.rdev();
            let major = (dev >> 8 & 0xfff) | (dev >> 32 & 0xfffff000);
            let minor = (dev & 0xff) | (dev >> 12 & 0xffffff00);
            if minor != index as u64 {
                continue;
            }
            let leaf = td_alloc::format(format_args!("/sys/dev/char/{major}:{minor}/device"))
                .map_err(|_| Error::Capacity)?;
            let Ok(leaf) = fs::canonicalize(leaf) else {
                continue;
            };
            if !leaf.starts_with("/sys/devices/virtual") {
                continue;
            }
            let mut buf = [0; 4097];
            let uevent = td_alloc::format(format_args!("{}/uevent", leaf.display()))
                .map_err(|_| Error::Capacity)?;
            let Ok(mut file) = File::open(&uevent) else {
                continue;
            };
            let mut read = 0;
            while let Some(left) = buf.get_mut(read..).filter(|s| !s.is_empty()) {
                let n = file.read(left)?;
                if n == 0 {
                    break;
                }
                read += n;
            }
            let Some(event) = buf
                .get(..read)
                .filter(|b| b.len() < 4097)
                .and_then(|b| std::str::from_utf8(b).ok())
            else {
                continue;
            };
            if !event
                .lines()
                .any(|line| line.strip_prefix("HID_UNIQ=") == Some(&self.identity))
                || !event.lines().any(|l| l == "HID_PHYS=td/firefox-passkeys")
                || !event.lines().any(|l| l == "HID_ID=0003:00001209:00007464")
            {
                continue;
            }
            let node = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(FLAGS)
                .open(&path)?;
            let pinned = node.metadata()?;
            if (meta.dev(), meta.ino(), meta.rdev()) != (pinned.dev(), pinned.ino(), pinned.rdev())
            {
                continue;
            }
            // All publishers hold the same /dev/uhid lock and use this
            // exclusive selector. A prior session's minor may be reused.
            match fs::symlink_metadata(SELECTOR) {
                Ok(link) if link.file_type().is_symlink() && link.uid() == 0 => {
                    fs::remove_file(SELECTOR)?;
                }
                Ok(_) => return Err(Error::Refused),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
            std::os::unix::fs::symlink(&name, SELECTOR)?;
            let selected = fs::metadata(SELECTOR)?;
            if (
                selected.dev(),
                selected.ino(),
                selected.rdev(),
                selected.nlink(),
            ) != (pinned.dev(), pinned.ino(), pinned.rdev(), 1)
            {
                return Err(Error::Stale);
            }
            node.set_permissions(fs::Permissions::from_mode(0o660))?;
            std::os::unix::fs::fchown(&node, None, Some(group))?;
            let granted = node.metadata()?;
            if (granted.gid(), granted.mode() & 0o7777, granted.nlink()) != (group, 0o660, 1) {
                return Err(Error::Stale);
            }
            self.node = Some(node);
            return Ok(true);
        }
        Ok(false)
    }
    pub fn input(&mut self, report: &Report) -> Result<()> {
        let mut event = [0; 70];
        put(&mut event, 0, &12u32.to_ne_bytes())?;
        put(&mut event, 4, &64u16.to_ne_bytes())?;
        put(&mut event, 6, report)?;
        write_event(&mut self.file, &event)
    }
    pub fn output(&mut self) -> Result<Option<Report>> {
        let mut event = [0; DESC_OFFSET + 4096];
        match self.file.read(&mut event) {
            Ok(0) => Err(Error::Stale),
            Ok(n) => decode_output(event.get(..n).ok_or(Error::Invalid)?),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}

fn decode_output(event: &[u8]) -> Result<Option<Report>> {
    let kind = u32::from_ne_bytes(
        event
            .get(..4)
            .ok_or(Error::Invalid)?
            .try_into()
            .map_err(|_| Error::Invalid)?,
    );
    if kind != 6 {
        return Ok(None);
    }
    // HID_OUTPUT_REPORT with the leading unnumbered report ID.
    if event.get(OUTPUT_SIZE..OUTPUT_SIZE + 2) != Some(&65u16.to_ne_bytes())
        || event.get(OUTPUT_SIZE + 2) != Some(&1)
        || event.get(4) != Some(&0)
    {
        return Ok(None);
    }
    Ok(Some(
        event
            .get(5..69)
            .ok_or(Error::Invalid)?
            .try_into()
            .map_err(|_| Error::Invalid)?,
    ))
}
