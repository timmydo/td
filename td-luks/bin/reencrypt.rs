//! Reencryption ownership, recovery and the online hotzone loop.
use crate::{dm, dm_sys, lock, reencrypt_io, reencrypt_stack as stack, Failure};
use dm_sys::{device_number, Control, Kind, Target};
use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::Instant;
use td_luks::header;
use td_luks::keyslot::DevRandom;
use td_luks::reencrypt::{self as meta, Hotzone, Keys, Resilience};
use td_luks::write::{self, Header, Plan, DATA_OFFSET};

type Result<T> = std::result::Result<T, Failure>;
struct Session {
    file: File,
    uuid: String,
    raw: (u32, u32),
    size: u64,
    _header: lock::Lock,
    _reencryption: lock::Lock,
}
impl Session {
    fn held(mut file: File, uuid: String) -> Result<Self> {
        let info = file.metadata()?;
        if !info.file_type().is_block_device() {
            return Err(Failure::new("reencryption requires a block device"));
        }
        let reencryption = lock::reencryption(&uuid)?;
        let header = lock::resource_in(Path::new(lock::LOCK_DIR), info.rdev(), lock::Mode::Write)?;
        if header::read_copy(&mut file)?.identity.uuid != uuid {
            return Err(Failure::new(
                "volume UUID changed while taking reencryption lock",
            ));
        }
        let size = file
            .seek(SeekFrom::End(0))?
            .checked_sub(DATA_OFFSET)
            .filter(|n| *n > 0 && n.is_multiple_of(4096))
            .ok_or_else(|| Failure::new("invalid data length"))?;
        Ok(Self {
            file,
            uuid,
            raw: device_number(info.rdev()),
            size,
            _header: header,
            _reencryption: reencryption,
        })
    }
    fn open(path: &Path) -> Result<Self> {
        let (mut file, read) = lock::discover(path)?;
        let uuid = header::read_copy(&mut file)?.identity.uuid;
        drop(read);
        Self::held(file, uuid)
    }
    fn names(&self, name: &str) -> Result<Vec<lock::Lock>> {
        let (overlay, hotzone) = stack::names(name)?;
        for (kind, name) in [
            ("LUKS2", name),
            ("SUBDEV", overlay.as_str()),
            ("SUBDEV", hotzone.as_str()),
        ] {
            stack::uuid(kind, &self.uuid, name)?;
        }
        Ok(vec![
            lock::mapping(name)?,
            lock::mapping(&overlay)?,
            lock::mapping(&hotzone)?,
        ])
    }
    fn only_holder(&self, expected: Option<u64>) -> Result<()> {
        let mut found = Vec::new();
        for entry in fs::read_dir(format!(
            "/sys/dev/block/{}:{}/holders",
            self.raw.0, self.raw.1
        ))? {
            let text = fs::read_to_string(entry?.path().join("dev"))?;
            let (major, minor) = text
                .trim()
                .split_once(':')
                .ok_or_else(|| Failure::new("invalid holder device number"))?;
            found.push((
                major
                    .parse::<u32>()
                    .map_err(|_| Failure::new("invalid holder major"))?,
                minor
                    .parse::<u32>()
                    .map_err(|_| Failure::new("invalid holder minor"))?,
            ));
        }
        let wanted: Vec<_> = expected.into_iter().map(device_number).collect();
        if found != wanted {
            return Err(Failure::new(
                "volume has other active mappings; close them before reencryption",
            ));
        }
        Ok(())
    }
    fn cleanup(&self, control: &Control, name: &str) -> Result<()> {
        let prefix = format!("CRYPT-TEMP-{}-td-luks-", self.uuid.replace('-', ""));
        for entry in fs::read_dir("/sys/block")? {
            let path = entry?.path();
            let Ok(uuid) = fs::read_to_string(path.join("dm/uuid")) else {
                continue;
            };
            if !uuid.trim().starts_with(&prefix) {
                continue;
            }
            let name = fs::read_to_string(path.join("dm/name"))?;
            let name = name.trim();
            let Some(status) = control.status(name)? else {
                continue;
            };
            if status.uuid != uuid.trim() {
                return Err(Failure::new("temporary mapping identity changed"));
            }
            let deps = control.dependencies(name)?;
            if deps.len() == 1 && deps.first().map(|d| device_number(*d)) == Some(self.raw) {
                control.remove(name)?;
            }
        }
        let (overlay, hotzone) = stack::names(name)?;
        for helper in [overlay, hotzone] {
            if let Some(status) = control.status(&helper)? {
                if status.uuid != stack::uuid("SUBDEV", &self.uuid, &helper)? {
                    return Err(Failure::new("reencryption helper name is occupied"));
                }
                // Busy helpers are still referenced by a retained stack. Do not
                // detach them: the operator must close that mapping or reboot.
                control.remove(&helper)?;
                dm::remove_entry(&helper, status.device)?;
            }
        }
        Ok(())
    }
    fn data(&self, control: &Control, keys: &Keys) -> Result<reencrypt_io::Data> {
        let temporary = |index: u8, key: &[u8; 64]| -> Result<File> {
            let name = format!("temporary-cryptsetup-{}-{index}", std::process::id());
            let uuid = format!(
                "CRYPT-TEMP-{}-td-luks-{}-{index}",
                self.uuid.replace('-', ""),
                std::process::id()
            );
            let status = control.create_crypt(&name, &uuid, self.size / 512, self.raw, key)?;
            let opened = (|| -> Result<File> {
                let (_, minor) = device_number(status.device);
                let file = File::options()
                    .read(true)
                    .write(true)
                    .custom_flags(0o440000)
                    .open(format!("/dev/dm-{minor}"))?;
                if !file.metadata()?.file_type().is_block_device()
                    || file.metadata()?.rdev() != status.device
                {
                    return Err(Failure::new("temporary node identity differs"));
                }
                control.defer_remove(&name)?;
                Ok(file)
            })();
            if opened.is_err() {
                control.remove(&name)?;
            }
            opened
        };
        let old = temporary(0, keys.old.bytes())?;
        let new = temporary(1, keys.new.bytes())?;
        let raw = File::options()
            .read(true)
            .write(true)
            .custom_flags(0o40000)
            .open(format!("/proc/self/fd/{}", self.file.as_raw_fd()))?;
        if raw.metadata()?.rdev() != self.file.metadata()?.rdev()
            || !raw.metadata()?.file_type().is_block_device()
        {
            return Err(Failure::new("raw data descriptor identity differs"));
        }
        Ok(reencrypt_io::Data {
            raw,
            old,
            new,
            size: self.size,
        })
    }
}
fn transition(header: &Header, json: td_json::Json) -> Result<Plan> {
    Ok(Plan {
        uuid: header.uuid.clone(),
        label: header.label.clone(),
        seqid: header
            .seqid
            .checked_add(1)
            .ok_or_else(|| Failure::new("sequence number overflow"))?,
        used: header.used,
        areas: Vec::new(),
        json,
    })
}

/// The caller released its initial shared header lock but retained this File.
pub fn open(file: File, uuid: String, name: &str, key: &[u8]) -> Result<()> {
    let mut session = Session::held(file, uuid)?;
    if write::load(&mut session.file).is_ok() {
        let (header, volume) = write::activation(
            &mut session.file,
            write::ActivationKey::Passphrase {
                slot: None,
                bytes: key,
            },
        )?;
        return dm::activate(&mut session.file, name, &header.uuid, volume.bytes())
            .map_err(Failure::from);
    }
    let _names = session.names(name)?;
    let control = Control::open()?;
    if control.status(name)?.is_some() {
        return Err(Failure::new("mapping already exists"));
    }
    let (header, mut layout) = meta::load(&mut session.file)?;
    let (_, keys) = meta::unlock(&mut session.file, &header, key)?;
    // Recovery may have loaded a predecessor's unsynced checksummed copy.
    session.file.sync_all()?;
    session.cleanup(&control, name)?;
    session.only_holder(None)?;
    if let Some(zone) = layout.hotzone {
        let data = session.data(&control, &keys)?;
        data.recover(&session.file, zone, layout.area)?;
        drop(data);
        let plan = transition(&header, meta::finish_hotzone(&header.json, session.size)?)?;
        meta::commit(&mut session.file, &mut DevRandom, &plan)?;
        if write::load(&mut session.file).is_ok() {
            write::kill_unbound(&mut session.file, &mut DevRandom, key)?;
            let status = control.create_crypt(
                name,
                &stack::uuid("LUKS2", &session.uuid, name)?,
                session.size / 512,
                session.raw,
                keys.new.bytes(),
            )?;
            return dm::publish(&control, &status, name).map_err(Failure::from);
        }
        layout = meta::load(&mut session.file)?.1;
    }
    let targets = stack::clean(&layout, session.raw, &keys)?;
    let status =
        control.create_table(name, &stack::uuid("LUKS2", &session.uuid, name)?, &targets)?;
    dm::publish(&control, &status, name)?;
    Ok(())
}

pub fn run(path: &Path, name: &str, key: &[u8], start: Option<(u8, Resilience)>) -> Result<()> {
    let mut session = Session::open(path)?;
    let _names = session.names(name)?;
    let control = Control::open()?;
    let (initial, keys) = if let Some((slot, resilience)) = start {
        let (plan, keys) =
            meta::plan_start(&mut session.file, &mut DevRandom, slot, key, resilience)?;
        (Some(plan), keys)
    } else {
        let (header, layout) = meta::load(&mut session.file)?;
        if layout.hotzone.is_some() {
            return Err(Failure::new(
                "close and reopen the mapping to recover its dirty hotzone",
            ));
        }
        let (_, keys) = meta::unlock(&mut session.file, &header, key)?;
        (None, keys)
    };
    // Persist the authenticated resume point before reusing its resilience area.
    session.file.sync_all()?;
    session.cleanup(&control, name)?;
    let top = control
        .status(name)?
        .ok_or_else(|| Failure::new("active mapping is absent"))?;
    session.only_holder(Some(top.device))?;
    if initial.is_some()
        && !control.matches_crypt(name, session.size / 512, session.raw, keys.old.bytes())?
    {
        return Err(Failure::new(
            "active mapping differs from the old volume key",
        ));
    }
    let original = if initial.is_some() {
        vec![Target {
            start: 0,
            length: session.size / 512,
            device: session.raw,
            offset: DATA_OFFSET / 512,
            kind: Kind::Crypt {
                key: keys.old.bytes(),
                iv: 0,
            },
        }]
    } else {
        let (_, layout) = meta::load(&mut session.file)?;
        stack::clean(&layout, session.raw, &keys)?
    };
    let data = session.data(&control, &keys)?;
    let online = stack::Stack::attach(
        &control,
        name,
        &session.uuid,
        session.raw,
        session.size,
        &original,
    )?;
    if let Some(plan) = initial {
        meta::commit(&mut session.file, &mut DevRandom, &plan)?;
    }
    let mut last = None::<Instant>;
    loop {
        let (header, layout) = meta::load(&mut session.file)?;
        progress(layout.progress, session.size, &mut last, false)?;
        let capacity = match layout.area.resilience {
            Resilience::Checksum => layout.area.size / 32 * 4096,
            Resilience::Journal => layout.area.size,
        };
        let length = (session.size - layout.progress)
            .min(meta::HOTZONE_SIZE)
            .min(capacity);
        let dirty = meta::mark_hotzone(&header.json, session.size, length)?;
        let dirty_layout = meta::profile(&dirty, session.size)?;
        let zone = Hotzone {
            offset: layout.progress,
            size: length,
        };
        online.pause(&dirty_layout, &keys)?;
        let protection = data.protect(zone, layout.area)?;
        let mut plan = transition(&header, dirty)?;
        plan.areas.push((layout.area.offset, protection));
        meta::commit(&mut session.file, &mut DevRandom, &plan)?;
        data.rewrite(zone)?;
        let (header, _) = meta::load(&mut session.file)?;
        let plan = transition(&header, meta::finish_hotzone(&header.json, session.size)?)?;
        meta::commit(&mut session.file, &mut DevRandom, &plan)?;
        online.release()?;
        if zone.offset + zone.size == session.size {
            break;
        }
    }
    online.finish(&keys.new)?;
    drop(data);
    write::kill_unbound(&mut session.file, &mut DevRandom, key)?;
    drop(keys);
    progress(session.size, session.size, &mut last, true)
}
fn progress(done: u64, size: u64, last: &mut Option<Instant>, finished: bool) -> Result<()> {
    if finished || last.is_none_or(|at| at.elapsed().as_secs() >= 1) {
        let mut out = io::stdout().lock();
        writeln!(
            out,
            "{{\"device_bytes\":\"{done}\",\"device_size\":\"{size}\"}}"
        )?;
        out.flush()?;
        *last = Some(Instant::now());
    }
    Ok(())
}
