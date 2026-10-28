//! Online device stack. Callers hold volume, reencryption and mapping locks.
use crate::dm;
use crate::dm_sys::{device_number, Control, Kind, Status, Target};
use std::io;
use td_luks::keyslot::Key;
use td_luks::reencrypt::{Keys, Layout};
use td_luks::write::DATA_OFFSET;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
pub fn uuid(kind: &str, uuid: &str, name: &str) -> io::Result<String> {
    let result = format!("CRYPT-{kind}-{}-{name}", uuid.replace('-', ""));
    if result.len() >= 129 {
        return Err(invalid("mapping UUID exceeds kernel field"));
    }
    Ok(result)
}
pub fn names(name: &str) -> io::Result<(String, String)> {
    let overlay = format!("{name}-overlay");
    let hotzone = format!("{name}-hotzone-forward");
    if !crate::dm_sys::valid_name(&overlay) || !crate::dm_sys::valid_name(&hotzone) {
        return Err(invalid("reencryption helper names exceed kernel fields"));
    }
    Ok((overlay, hotzone))
}
pub fn clean<'a>(layout: &Layout, raw: (u32, u32), keys: &'a Keys) -> io::Result<Vec<Target<'a>>> {
    if layout.hotzone.is_some() {
        return Err(invalid("recover dirty metadata before activation"));
    }
    let mut out = Vec::new();
    if layout.progress != 0 {
        out.push(Target {
            start: 0,
            length: layout.progress / 512,
            device: raw,
            offset: DATA_OFFSET / 512,
            kind: Kind::Crypt {
                key: keys.new.bytes(),
                iv: 0,
            },
        });
    }
    let at = layout.progress / 512;
    let length = layout
        .data_size
        .checked_sub(layout.progress)
        .ok_or_else(|| invalid("progress exceeds size"))?
        / 512;
    if length != 0 {
        out.push(Target {
            start: at,
            length,
            device: raw,
            offset: DATA_OFFSET / 512 + at,
            kind: Kind::Crypt {
                key: keys.old.bytes(),
                iv: at,
            },
        });
    }
    Ok(out)
}
fn remove_helper(control: &Control, status: &Status) -> io::Result<()> {
    let current = control
        .status(&status.name)?
        .ok_or_else(|| invalid("helper disappeared"))?;
    if current.device != status.device || current.uuid != status.uuid {
        return Err(invalid("helper identity changed"));
    }
    control.remove(&status.name)?;
    dm::remove_entry(&status.name, status.device)
}
pub struct Stack<'a> {
    control: &'a Control,
    name: String,
    overlay: Status,
    hotzone: Status,
    raw: (u32, u32),
    size: u64,
}
impl<'a> Stack<'a> {
    pub fn attach(
        control: &'a Control,
        name: &str,
        volume_uuid: &str,
        raw: (u32, u32),
        size: u64,
        original: &[Target<'_>],
    ) -> io::Result<Self> {
        let (overlay_name, hotzone_name) = names(name)?;
        let overlay_uuid = uuid("SUBDEV", volume_uuid, &overlay_name)?;
        let hotzone_uuid = uuid("SUBDEV", volume_uuid, &hotzone_name)?;
        let top = control
            .status(name)?
            .ok_or_else(|| invalid("active mapping is absent"))?;
        if top.uuid != uuid("LUKS2", volume_uuid, name)?
            || !top.active
            || top.suspended
            || !control.matches_table(name, original)?
        {
            return Err(invalid(
                "active mapping does not match authenticated metadata",
            ));
        }
        let linear = [Target {
            start: 0,
            length: size / 512,
            device: raw,
            offset: DATA_OFFSET / 512,
            kind: Kind::Linear,
        }];
        let hotzone = control.create_table(&hotzone_name, &hotzone_uuid, &linear)?;
        dm::publish(control, &hotzone, &hotzone_name)?;
        let overlay = match control.create_table(&overlay_name, &overlay_uuid, original) {
            Ok(status) => status,
            Err(error) => {
                remove_helper(control, &hotzone)?;
                return Err(error);
            }
        };
        if let Err(error) = dm::publish(control, &overlay, &overlay_name) {
            remove_helper(control, &hotzone)?;
            return Err(error);
        }
        let top_table = [Target {
            start: 0,
            length: size / 512,
            device: device_number(overlay.device),
            offset: 0,
            kind: Kind::Linear,
        }];
        let switched = (|| {
            control.load_table(name, &top_table)?;
            control.set_suspended(name, true)?;
            control.set_suspended(name, false)?;
            if !control.matches_table(name, &top_table)? {
                return Err(invalid("top mapping switch differs"));
            }
            Ok(())
        })();
        if let Err(error) = switched {
            // No data has changed yet. Clear any inactive helper reference
            // by replacing it with the original table before cleanup.
            control.load_table(name, original)?;
            control.set_suspended(name, true)?;
            control.set_suspended(name, false)?;
            remove_helper(control, &overlay)?;
            remove_helper(control, &hotzone)?;
            return Err(error);
        }
        Ok(Self {
            control,
            name: name.into(),
            overlay,
            hotzone,
            raw,
            size,
        })
    }
    pub fn pause(&self, dirty: &Layout, keys: &Keys) -> io::Result<()> {
        let zone = dirty
            .hotzone
            .ok_or_else(|| invalid("no hotzone to pause"))?;
        let mut targets = Vec::new();
        let start = zone.offset / 512;
        let length = zone.size / 512;
        if start != 0 {
            targets.push(Target {
                start: 0,
                length: start,
                device: self.raw,
                offset: DATA_OFFSET / 512,
                kind: Kind::Crypt {
                    key: keys.new.bytes(),
                    iv: 0,
                },
            });
        }
        targets.push(Target {
            start,
            length,
            device: device_number(self.hotzone.device),
            offset: start,
            kind: Kind::Crypt {
                key: keys.new.bytes(),
                iv: start,
            },
        });
        let end = start
            .checked_add(length)
            .ok_or_else(|| invalid("hotzone overflow"))?;
        let remaining = (self.size / 512)
            .checked_sub(end)
            .ok_or_else(|| invalid("hotzone exceeds device"))?;
        if remaining != 0 {
            targets.push(Target {
                start: end,
                length: remaining,
                device: self.raw,
                offset: DATA_OFFSET / 512 + end,
                kind: Kind::Crypt {
                    key: keys.old.bytes(),
                    iv: end,
                },
            });
        }
        self.control.load_table(&self.overlay.name, &targets)?;
        // Suspend top to bottom. Reversing these can deadlock outstanding I/O.
        self.control.set_suspended(&self.overlay.name, true)?;
        self.control.set_suspended(&self.hotzone.name, true)?;
        self.control.set_suspended(&self.overlay.name, false)?;
        if !self.control.matches_table(&self.overlay.name, &targets)? {
            return Err(invalid("paused overlay table differs"));
        }
        Ok(())
    }
    pub fn release(&self) -> io::Result<()> {
        self.control.set_suspended(&self.hotzone.name, false)
    }
    pub fn finish(self, key: &Key) -> io::Result<()> {
        let table = [Target {
            start: 0,
            length: self.size / 512,
            device: self.raw,
            offset: DATA_OFFSET / 512,
            kind: Kind::Crypt {
                key: key.bytes(),
                iv: 0,
            },
        }];
        self.control.load_table(&self.name, &table)?;
        self.control.set_suspended(&self.name, true)?;
        self.control.set_suspended(&self.name, false)?;
        if !self.control.matches_table(&self.name, &table)? {
            return Err(invalid("final table differs"));
        }
        remove_helper(self.control, &self.overlay)?;
        remove_helper(self.control, &self.hotzone)
    }
}
