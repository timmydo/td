//! One immutable timezone snapshot per compositor session.

use crate::timezone::{civil_from_days, Zone, MAX_BYTES};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

#[derive(Debug)]
pub enum Clock {
    Utc,
    Local(Zone),
    Unavailable,
}

impl Clock {
    pub fn load(setting: &Path, zoneinfo: &Path) -> Result<Self, String> {
        let metadata = match fs::metadata(setting) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::Utc),
            Err(error) => return Err(format!("read timezone setting: {error}")),
        };
        if !metadata.is_file() {
            return Err("timezone setting is not a regular file".into());
        }
        let bytes = read_bounded(setting, 65)?;
        let name = std::str::from_utf8(&bytes).map_err(|_| "timezone setting is not UTF-8")?;
        let name = name.strip_suffix('\n').unwrap_or(name);
        let components: Vec<_> = name.split('/').collect();
        if name.len() > 64
            || !(2..=3).contains(&components.len())
            || components.iter().any(|part| {
                !part.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
                    || !part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_+-".contains(&byte))
            })
        {
            return Err("invalid timezone name".into());
        }
        // The deployment's zoneinfo root is a store symlink. Its packaged
        // descendants are directories and regular files, never symlinks.
        let mut path = zoneinfo.to_path_buf();
        for (index, component) in components.iter().enumerate() {
            path.push(component);
            let kind = fs::symlink_metadata(&path)
                .map_err(|error| format!("read selected timezone: {error}"))?;
            let last = index + 1 == components.len();
            if (last && !kind.is_file()) || (!last && !kind.is_dir()) {
                return Err("selected timezone has a non-regular path".into());
            }
        }
        let bytes = read_bounded(&path, MAX_BYTES)?;
        let zone = Zone::parse(&bytes).ok_or("invalid or unsupported timezone data")?;
        Ok(Self::Local(zone))
    }

    pub fn stamp(&self, epoch: Option<u64>) -> String {
        self.try_stamp(epoch).unwrap_or_else(|| "CLOCK ?".into())
    }

    fn try_stamp(&self, epoch: Option<u64>) -> Option<String> {
        let epoch = i64::try_from(epoch?).ok()?;
        let offset = match self {
            Self::Utc => 0,
            Self::Local(zone) => zone.offset_at(epoch)?,
            Self::Unavailable => return None,
        };
        let local = epoch.checked_add(i64::from(offset))?;
        let (year, month, day) = civil_from_days(local.div_euclid(86_400));
        let seconds = local.rem_euclid(86_400);
        let (hour, minute, second) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
        let suffix = if offset == 0 {
            "UTC".into()
        } else {
            let magnitude = offset.unsigned_abs();
            let sign = if offset < 0 { '-' } else { '+' };
            let mut suffix = format!(
                "UTC{sign}{:02}:{:02}",
                magnitude / 3600,
                magnitude / 60 % 60
            );
            if magnitude % 60 != 0 {
                suffix.push_str(&format!(":{:02}", magnitude % 60));
            }
            suffix
        };
        Some(format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} {suffix}"
        ))
    }
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|error| format!("open timezone data: {error}"))?;
    if !file
        .metadata()
        .map_err(|error| format!("inspect timezone data: {error}"))?
        .is_file()
    {
        return Err("timezone data is not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read timezone data: {error}"))?;
    if bytes.len() > limit {
        return Err("timezone data exceeds its size limit".into());
    }
    Ok(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::timezone::fixture;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "td-clock-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn setting(&self) -> std::path::PathBuf {
            self.0.join("timezone")
        }
        fn zones(&self) -> std::path::PathBuf {
            self.0.join("zoneinfo")
        }
        fn load(&self) -> Result<Clock, String> {
            Clock::load(&self.setting(), &self.zones())
        }
        fn seed(&self) {
            fs::create_dir_all(self.zones().join("Europe")).unwrap();
            fs::write(
                self.zones().join("Europe/London"),
                fixture(&[], &[(0, false, "GMT")], "GMT0BST,M3.5.0/1,M10.5.0"),
            )
            .unwrap();
            fs::write(self.setting(), b"Europe/London\n").unwrap();
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_installed_links_load_once_and_the_clock_follows_dst() {
        let scratch = Scratch::new();
        scratch.seed();
        fs::rename(scratch.setting(), scratch.0.join("saved")).unwrap();
        symlink("saved", scratch.setting()).unwrap();
        fs::rename(scratch.zones(), scratch.0.join("immutable")).unwrap();
        symlink("immutable", scratch.zones()).unwrap();
        let clock = scratch.load().unwrap();
        fs::write(scratch.0.join("saved"), "broken").unwrap();
        fs::remove_file(scratch.zones().join("Europe/London")).unwrap();
        assert_eq!(clock.stamp(Some(4_102_444_800)), "2100-01-01 00:00:00 UTC");
        assert_eq!(
            clock.stamp(Some(4_118_083_200)),
            "2100-07-01 01:00:00 UTC+01:00"
        );
        assert!(scratch.load().is_err());
    }

    #[test]
    fn an_absent_setting_is_utc_but_invalid_settings_refuse() {
        let scratch = Scratch::new();
        assert!(matches!(scratch.load().unwrap(), Clock::Utc));
        scratch.seed();
        for setting in [
            "",
            "UTC\n",
            "../Europe/London\n",
            "Europe/../London\n",
            "Europe/London\n\n",
            " Europe/London\n",
            "/Europe/London\n",
            "europe/London\n",
            "Europe/London\r\n",
            "Europe//London",
            "Europe/London/Too/Deep",
        ] {
            fs::write(scratch.setting(), setting).unwrap();
            assert!(scratch.load().is_err(), "{setting:?}");
        }
        for bytes in [vec![b'A'; 66], vec![0xff]] {
            fs::write(scratch.setting(), bytes).unwrap();
            assert!(scratch.load().is_err());
        }
        fs::remove_file(scratch.setting()).unwrap();
        fs::create_dir(scratch.setting()).unwrap();
        assert!(scratch.load().is_err());
    }

    #[test]
    fn selected_zone_missing_corrupt_oversized_or_symlinked_refuses() {
        let scratch = Scratch::new();
        scratch.seed();
        let zone = scratch.zones().join("Europe/London");
        fs::remove_file(&zone).unwrap();
        assert!(scratch.load().is_err());
        for bytes in [b"TZif3".to_vec(), vec![0; MAX_BYTES + 1]] {
            fs::write(&zone, bytes).unwrap();
            assert!(scratch.load().is_err());
        }
        fs::remove_file(&zone).unwrap();
        fs::write(
            scratch.0.join("outside"),
            fixture(&[], &[(0, false, "UTC")], "UTC0"),
        )
        .unwrap();
        symlink(scratch.0.join("outside"), &zone).unwrap();
        assert!(scratch.load().is_err());
        fs::remove_file(&zone).unwrap();
        fs::remove_dir(scratch.zones().join("Europe")).unwrap();
        symlink(&scratch.0, scratch.zones().join("Europe")).unwrap();
        assert!(scratch.load().is_err());
    }

    #[test]
    fn numeric_offsets_include_fractional_hours_and_previous_civil_days() {
        for (footer, expected) in [
            ("EST5", "1969-12-31 19:00:00 UTC-05:00"),
            ("<+0545>-5:45", "1970-01-01 05:45:00 UTC+05:45"),
            ("LMT-0:09:21", "1970-01-01 00:09:21 UTC+00:09:21"),
        ] {
            let clock =
                Clock::Local(Zone::parse(&fixture(&[], &[(0, false, "UTC")], footer)).unwrap());
            let text = clock.stamp(Some(0));
            assert_eq!(text, expected);
            assert!(text.bytes().all(crate::ui::is_mapped));
        }
        for (epoch, expected) in [
            (0, "1970-01-01 00:00:00 UTC"),
            (86_399, "1970-01-01 23:59:59 UTC"),
            (86_400, "1970-01-02 00:00:00 UTC"),
            (68_255_999, "1972-02-29 23:59:59 UTC"),
            (951_782_400, "2000-02-29 00:00:00 UTC"),
            (4_107_542_400, "2100-03-01 00:00:00 UTC"),
            (4_107_456_000, "2100-02-28 00:00:00 UTC"),
            (1_770_000_000, "2026-02-02 02:40:00 UTC"),
        ] {
            assert_eq!(Clock::Utc.stamp(Some(epoch)), expected);
        }
        assert_eq!(Clock::Unavailable.stamp(Some(0)), "CLOCK ?");
        assert_eq!(Clock::Utc.stamp(None), "CLOCK ?");
        assert_eq!(Clock::Utc.stamp(Some(u64::MAX)), "CLOCK ?");
        assert_eq!(
            Clock::Local(Zone::parse(&fixture(&[], &[(0, false, "UTC")], "<-00>0")).unwrap())
                .stamp(Some(0)),
            "CLOCK ?"
        );
    }
}
