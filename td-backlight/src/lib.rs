//! Panel selection shared by the seat assigner and compositor. Sysfs parents
//! are kernel-owned; only the selected brightness attribute is delegated.
#![forbid(unsafe_code)]

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const MAX_DEVICES: usize = 64;
const MIN_PERCENT: u32 = 5;
const START_PERCENT: u32 = 60;
const STEP_PERCENT: u32 = 5;

#[derive(Debug)]
pub struct Panel {
    directory: PathBuf,
    maximum: u32,
}

fn text(path: &Path) -> io::Result<String> {
    let mut value = String::new();
    File::open(path)?.take(129).read_to_string(&mut value)?;
    if value.len() > 128 {
        return Err(io::Error::other("backlight attribute exceeds 128 bytes"));
    }
    Ok(value.trim().to_owned())
}

fn number(path: &Path) -> io::Result<u32> {
    let value = text(path)?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(io::Error::other("invalid backlight numeric attribute"));
    }
    value.parse().map_err(io::Error::other)
}

fn units(maximum: u32, percent: u32) -> u32 {
    (u64::from(maximum) * u64::from(percent)).div_ceil(100) as u32
}

impl Panel {
    pub fn brightness_path(&self) -> PathBuf {
        self.directory.join("brightness")
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn current(&self) -> io::Result<u32> {
        let current = number(&self.brightness_path())?;
        if current > self.maximum {
            return Err(io::Error::other("brightness exceeds panel maximum"));
        }
        Ok(current)
    }

    fn apply(&self, value: u32) -> io::Result<u32> {
        let value = value.clamp(units(self.maximum, MIN_PERCENT), self.maximum);
        fs::write(self.brightness_path(), format!("{value}\n"))?;
        let actual = self.current()?;
        if actual != value {
            return Err(io::Error::other(format!(
                "backlight readback {actual} differs from requested {value}"
            )));
        }
        Ok(((u64::from(actual) * 100) / u64::from(self.maximum)) as u32)
    }

    /// Called after the modeset, so a firmware/driver transition cannot leave
    /// the panel at a nearly dark inherited setting. Brighter settings survive.
    pub fn ensure_visible(&self) -> io::Result<u32> {
        self.apply(self.current()?.max(units(self.maximum, START_PERCENT)))
    }

    pub fn adjust(&self, brighter: bool) -> io::Result<u32> {
        let current = self.current()?;
        let step = units(self.maximum, STEP_PERCENT);
        let next = if brighter {
            current.saturating_add(step)
        } else {
            current.saturating_sub(step)
        };
        self.apply(next)
    }
}

pub fn discover() -> io::Result<Option<Panel>> {
    discover_in(Path::new("/sys/class/backlight"), Path::new("/sys/devices"))
}

/// Both roots must be trusted. The class's expected symlinks may resolve only
/// beneath the device tree; attributes themselves must be regular files.
pub fn discover_in(class: &Path, devices: &Path) -> io::Result<Option<Panel>> {
    let entries = match fs::read_dir(class) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let devices = fs::canonicalize(devices)?;
    let mut candidates = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_DEVICES {
            return Err(io::Error::other("too many backlight devices"));
        }
        let entry = entry?;
        let directory = fs::canonicalize(entry.path())?;
        if !directory.starts_with(&devices) || directory == devices {
            return Err(io::Error::other(
                "backlight class entry escapes the device tree",
            ));
        }
        for name in ["brightness", "max_brightness", "type"] {
            if !fs::symlink_metadata(directory.join(name))?
                .file_type()
                .is_file()
            {
                return Err(io::Error::other(
                    "backlight attribute is not a regular file",
                ));
            }
        }
        let priority = match text(&directory.join("type"))?.as_str() {
            "raw" => 0,
            "platform" => 1,
            "firmware" => 2,
            _ => continue,
        };
        let maximum = number(&directory.join("max_brightness"))?;
        if maximum == 0 {
            return Err(io::Error::other("backlight maximum is zero"));
        }
        candidates.push((priority, entry.file_name(), Panel { directory, maximum }));
    }
    candidates.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    Ok(candidates.into_iter().next().map(|(_, _, panel)| panel))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::os::unix::fs::symlink;
    use td_fs::TempDir;

    fn fixture() -> TempDir {
        let root = TempDir::new("td-backlight").unwrap();
        fs::create_dir(root.path().join("class")).unwrap();
        fs::create_dir(root.path().join("devices")).unwrap();
        root
    }

    fn panel(root: &Path, name: &str, kind: &str, maximum: u32, current: u32) -> PathBuf {
        let path = root.join("devices").join(name);
        fs::create_dir(&path).unwrap();
        fs::write(path.join("type"), kind).unwrap();
        fs::write(path.join("max_brightness"), maximum.to_string()).unwrap();
        fs::write(path.join("brightness"), current.to_string()).unwrap();
        symlink(&path, root.join("class").join(name)).unwrap();
        path
    }

    #[test]
    fn native_panel_wins_over_firmware_and_is_readable_after_modeset() {
        let root = fixture();
        let firmware = panel(root.path(), "acpi_video0", "firmware", 15, 1);
        let native = panel(root.path(), "intel_backlight", "raw", 976, 5);
        let chosen = discover_in(&root.path().join("class"), &root.path().join("devices"))
            .unwrap()
            .unwrap();
        assert_eq!(chosen.directory(), native);
        assert_eq!(chosen.ensure_visible().unwrap(), 60);
        assert_eq!(number(&firmware.join("brightness")).unwrap(), 1);
        assert!(chosen.adjust(true).unwrap() >= 65);
        for _ in 0..30 {
            chosen.adjust(false).unwrap();
        }
        assert!(number(&native.join("brightness")).unwrap() > 0);
        for _ in 0..30 {
            chosen.adjust(true).unwrap();
        }
        assert_eq!(chosen.adjust(true).unwrap(), 100);
    }

    #[test]
    fn brighter_boot_settings_and_coarse_panels_remain_usable() {
        let root = fixture();
        panel(root.path(), "coarse", "raw", 3, 3);
        let chosen = discover_in(&root.path().join("class"), &root.path().join("devices"))
            .unwrap()
            .unwrap();
        assert_eq!(chosen.ensure_visible().unwrap(), 100);
        for _ in 0..10 {
            chosen.adjust(false).unwrap();
        }
        assert!(chosen.current().unwrap() > 0);
    }

    #[test]
    fn numeric_limits_and_malformed_attributes_cannot_wrap_or_allocate_unboundedly() {
        let root = fixture();
        let native = panel(root.path(), "native", "raw", u32::MAX, 0);
        let class = root.path().join("class");
        let devices = root.path().join("devices");
        let chosen = discover_in(&class, &devices).unwrap().unwrap();
        assert_eq!(chosen.ensure_visible().unwrap(), 60);
        fs::write(native.join("brightness"), "x").unwrap();
        assert!(chosen.adjust(true).is_err());
        fs::write(native.join("max_brightness"), "9".repeat(129)).unwrap();
        assert!(discover_in(&class, &devices).is_err());
        fs::write(native.join("max_brightness"), "0").unwrap();
        assert!(discover_in(&class, &devices).is_err());
    }

    #[test]
    fn no_panel_is_optional_but_escape_and_attribute_symlink_are_refused() {
        let root = fixture();
        let class = root.path().join("class");
        let devices = root.path().join("devices");
        assert!(discover_in(&class, &devices).unwrap().is_none());
        let native = panel(root.path(), "native", "raw", 100, 1);
        fs::remove_file(native.join("brightness")).unwrap();
        symlink(native.join("max_brightness"), native.join("brightness")).unwrap();
        assert!(discover_in(&class, &devices).is_err());
        fs::remove_file(class.join("native")).unwrap();
        symlink(root.path(), class.join("outside")).unwrap();
        assert!(discover_in(&class, &devices).is_err());
    }
}
