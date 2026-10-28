//! Deployment labels, appended before the deployment's manifest is written.
//! The target recipe output stays unchanged; assembly adds only a data archive.
use super::cpio;
use std::io::{self, Read, Seek, SeekFrom, Write};

#[path = "../../td-civil/src/tzif.rs"]
#[allow(dead_code, reason = "shared Gregorian conversion for deployment dates")]
mod calendar;

pub const MEMBER: &str = "td-build-info";
pub const ARCHIVE_BYTES: usize = 512;
const HEADER: &str = "td-build-info-v1";
const TEXT_LIMIT: usize = 192;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Info {
    pub commit: String,
    pub commit_date: String,
    pub build_date: String,
    pub dirty: bool,
}

fn date_valid(date: &str) -> bool {
    let Some((y, rest)) = date.split_once('-') else {
        return false;
    };
    let Some((m, d)) = rest.split_once('-') else {
        return false;
    };
    if y.len() != 4
        || m.len() != 2
        || d.len() != 2
        || !y
            .bytes()
            .chain(m.bytes())
            .chain(d.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return false;
    }
    let (Ok(y), Ok(m), Ok(d)) = (y.parse::<i64>(), m.parse::<u8>(), d.parse::<u8>()) else {
        return false;
    };
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return false;
    }
    let Ok(days) = i64::try_from(calendar::days_from_civil(y, m, d)) else {
        return false;
    };
    calendar::civil_from_days(days) == (y, m, d)
}

/// UTC calendar date, independent of the build machine's timezone.
pub fn utc_date(seconds: u64) -> Result<String, String> {
    let days = i64::try_from(seconds / 86_400).map_err(|_| "build date overflow")?;
    let (year, month, day) = calendar::civil_from_days(days);
    if !(1970..=9999).contains(&year) {
        return Err("build date outside supported years".into());
    }
    Ok(format!("{year:04}-{month:02}-{day:02}"))
}

impl Info {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        if !matches!(self.commit.len(), 40 | 64)
            || !self
                .commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("build commit must be a full lowercase Git object ID".into());
        }
        if !date_valid(&self.commit_date) || !date_valid(&self.build_date) {
            return Err("invalid build information date".into());
        }
        let state = if self.dirty { "dirty" } else { "clean" };
        let text = format!(
            "{HEADER}\ncommit={}\ncommit-date={}\nbuild-date={}\nsource={state}\n",
            self.commit, self.commit_date, self.build_date
        );
        if text.len() > TEXT_LIMIT {
            return Err("build information exceeds its bound".into());
        }
        Ok(text.into_bytes())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > TEXT_LIMIT {
            return Err("build information exceeds its bound".into());
        }
        let text = std::str::from_utf8(bytes).map_err(|_| "build information is not UTF-8")?;
        let mut rows = text.split('\n');
        if rows.next() != Some(HEADER) {
            return Err("unsupported build information".into());
        }
        let mut field = |name: &str| {
            rows.next()
                .and_then(|row| row.strip_prefix(name))
                .map(str::to_string)
                .ok_or_else(|| format!("missing build field {name}"))
        };
        let commit = field("commit=")?;
        let commit_date = field("commit-date=")?;
        let build_date = field("build-date=")?;
        let dirty = match field("source=")?.as_str() {
            "clean" => false,
            "dirty" => true,
            _ => return Err("invalid source state".into()),
        };
        if rows.next() != Some("") || rows.next().is_some() {
            return Err("trailing build information".into());
        }
        let value = Self {
            commit,
            commit_date,
            build_date,
            dirty,
        };
        if value.encode()? != bytes {
            return Err("noncanonical build information".into());
        }
        Ok(value)
    }

    pub fn label(&self) -> Result<String, String> {
        self.encode()?;
        let short = self.commit.get(..12).ok_or("short commit unavailable")?;
        let state = if self.dirty { "+dirty" } else { "" };
        Ok(format!(
            "{} {}{}  built {} UTC",
            self.commit_date, short, state, self.build_date
        ))
    }

    pub fn archive(&self) -> Result<Vec<u8>, String> {
        let data = self.encode()?;
        let mut archive = cpio::build(&[cpio::Entry {
            name: MEMBER,
            mode: 0o444,
            kind: cpio::Kind::File(&data),
        }])?;
        if archive.len() > ARCHIVE_BYTES {
            return Err("build information archive exceeds its bound".into());
        }
        archive.resize(ARCHIVE_BYTES, 0);
        Ok(archive)
    }
}

/// Read only the fixed final appendix from an already verified initramfs.
/// A legacy initramfs has no such appendix. Never guess dates from inode times.
pub fn read(image: &mut (impl Read + Seek)) -> io::Result<Option<Info>> {
    let saved = image.stream_position()?;
    let result = (|| {
        let length = image.seek(SeekFrom::End(0))?;
        if length < ARCHIVE_BYTES as u64 || length % 4 != 0 {
            return Ok(None);
        }
        image.seek(SeekFrom::End(-(ARCHIVE_BYTES as i64)))?;
        let mut bytes = [0u8; ARCHIVE_BYTES];
        image.read_exact(&mut bytes)?;
        // Compare the entire canonical archive, not merely a header or name.
        let prefix = 110 + MEMBER.len() + 1;
        let data_at = prefix + cpio::alignment_padding(prefix);
        let Some(data) = bytes.get(data_at..) else {
            return Ok(None);
        };
        let Some(end) = data.iter().position(|b| *b == 0) else {
            return Ok(None);
        };
        let Some(text) = data.get(..end) else {
            return Ok(None);
        };
        let Ok(info) = Info::decode(text) else {
            return Ok(None);
        };
        match info.archive() {
            Ok(canonical) if canonical == bytes => Ok(Some(info)),
            _ => Ok(None),
        }
    })();
    let restore = image.seek(SeekFrom::Start(saved));
    match (result, restore) {
        (Ok(value), Ok(_)) => Ok(value),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

/// Copy a recipe initramfs and append the data before hashing it.
pub fn append(input: &mut impl Read, output: &mut impl Write, info: &Info) -> io::Result<u64> {
    let archive = info.archive().map_err(io::Error::other)?;
    let length = io::copy(input, output)?;
    let pad = (4 - length % 4) % 4;
    for _ in 0..pad {
        output.write_all(&[0])?;
    }
    output.write_all(&archive)?;
    length
        .checked_add(pad)
        .and_then(|len| len.checked_add(ARCHIVE_BYTES as u64))
        .ok_or_else(|| io::Error::other("initramfs size overflow"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use std::io::Cursor;
    fn info() -> Info {
        Info {
            commit: "a".repeat(40),
            commit_date: "2026-10-25".into(),
            build_date: "2026-10-26".into(),
            dirty: false,
        }
    }
    #[test]
    fn labels_round_trip_only_inside_the_exact_archive() {
        for length in 0..8 {
            let value = info();
            let mut image = Vec::new();
            append(&mut Cursor::new(vec![1; length]), &mut image, &value).unwrap();
            let mut cursor = Cursor::new(image.clone());
            cursor.set_position(1);
            assert_eq!(read(&mut cursor).unwrap(), Some(value));
            assert_eq!(cursor.position(), 1);
            for byte in image.len() - ARCHIVE_BYTES..image.len() {
                let mut corrupt = image.clone();
                corrupt[byte] ^= 1;
                assert_ne!(read(&mut Cursor::new(corrupt)).unwrap(), Some(info()));
            }
        }
    }
    #[test]
    fn legacy_and_untrusted_fields_have_no_invented_identity() {
        assert_eq!(read(&mut Cursor::new(vec![0; 2048])).unwrap(), None);
        for date in [
            "2026-02-30",
            "2026-13-01",
            "2026-00-01",
            "2026-01-00",
            "2026-1-01",
            "2026-10-25\x1b",
            "1969-12-31",
        ] {
            let mut value = info();
            value.commit_date = date.into();
            assert!(value.encode().is_err());
        }
        let mut value = info();
        value.commit = "f".repeat(64);
        value.dirty = true;
        assert!(value.label().unwrap().contains("ffffffffffff+dirty"));
        assert_eq!(Info::decode(&value.encode().unwrap()).unwrap(), value);
    }
    #[test]
    fn dates_are_utc_and_bounded() {
        assert_eq!(utc_date(0).unwrap(), "1970-01-01");
        assert_eq!(utc_date(1_709_164_800).unwrap(), "2024-02-29");
        assert!(utc_date(u64::MAX).is_err());
    }
}
