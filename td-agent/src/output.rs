//! A background process's output as its conversation keeps it (DESIGN.md
//! §12): in the conversation's `processes` directory, as segment files
//! each named for the byte offset it begins at, counted from the
//! process's start; past `background_output_bytes` the oldest are
//! dropped. It is read by offset. An offset counts the output as kept:
//! text, each byte the process wrote that was not UTF-8 replaced.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

// A segment is never opened through a link.
use crate::store::O_NOFOLLOW;

/// The directory, in a conversation's, its processes' output is kept in.
pub const DIR: &str = "processes";
/// The segments a process's retained output is cut into.
const SEGMENTS: u64 = 4;
/// `process_output`'s read, by default and at most.
pub const READ_BYTES: usize = 32 * 1024;
pub const MAX_READ_BYTES: usize = 100 * 1024;
/// How often a read retries when a segment it listed was dropped first.
const READ_TRIES: usize = 3;

/// A segment's file name: the process and the offset it begins at.
fn name(number: u64, start: u64) -> String {
    format!("p{number}-{start:020}")
}

/// Writes process `number`'s output as it comes, keeping at most `cap`
/// bytes of it, the latest.
pub struct Writer {
    dir: PathBuf,
    number: u64,
    cap: u64,
    segment: u64,
    file: Option<File>,
    /// The segments kept, each its start and length, oldest first.
    kept: VecDeque<(u64, u64)>,
    total: u64,
}

impl Writer {
    /// A writer for process `number` in `dir`, made private if it is not
    /// there; a process's number is never reused, so it has no output yet.
    pub fn create(dir: &Path, number: u64, cap: u64) -> Result<Self, String> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        // A process numbered so whose start a crash kept from the log.
        for (start, _) in segments(dir, number)? {
            let path = dir.join(name(number, start));
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            number,
            cap: cap.max(1),
            segment: (cap / SEGMENTS).max(1),
            file: None,
            kept: VecDeque::new(),
            total: 0,
        })
    }

    /// The bytes written, dropped ones included.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Takes the next bytes.
    pub fn push(&mut self, mut bytes: &[u8]) -> Result<(), String> {
        while !bytes.is_empty() {
            let full = self.kept.back().is_none_or(|(_, len)| *len >= self.segment);
            if self.file.is_none() || full {
                self.rotate()?;
            }
            let used = self.kept.back().map_or(0, |(_, len)| *len);
            let room = usize::try_from(self.segment.saturating_sub(used)).unwrap_or(usize::MAX);
            let (now, rest) = bytes.split_at(room.min(bytes.len()));
            let file = self.file.as_mut().ok_or("no segment is open")?;
            file.write_all(now)
                .map_err(|e| format!("p{}'s output: {e}", self.number))?;
            let wrote = now.len() as u64;
            if let Some((_, len)) = self.kept.back_mut() {
                *len = len.saturating_add(wrote);
            }
            self.total = self.total.saturating_add(wrote);
            bytes = rest;
            self.drop_oldest()?;
        }
        Ok(())
    }

    /// A new segment, beginning where the output is.
    fn rotate(&mut self) -> Result<(), String> {
        let path = self.dir.join(name(self.number, self.total));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        self.file = Some(file);
        self.kept.push_back((self.total, 0));
        Ok(())
    }

    /// The oldest segments dropped while the rest hold more than the cap;
    /// the one being written is kept.
    fn drop_oldest(&mut self) -> Result<(), String> {
        while self.kept.len() > 1 && self.kept.iter().map(|(_, len)| len).sum::<u64>() > self.cap {
            let Some((start, _)) = self.kept.pop_front() else {
                break;
            };
            let path = self.dir.join(name(self.number, start));
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        Ok(())
    }
}

/// A read of a process's output.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Output {
    /// The bytes from `from` to `to`, cut between characters.
    pub text: String,
    pub from: u64,
    pub to: u64,
    /// Where what is retained begins, and the bytes written in all.
    pub start: u64,
    pub total: u64,
}

/// Process `number`'s segments in `dir`: each its start and length, in
/// order; none when it wrote nothing.
fn segments(dir: &Path, number: u64) -> Result<Vec<(u64, u64)>, String> {
    let prefix = format!("p{number}-");
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let file_name = entry.file_name();
        let Some(start) = file_name
            .to_str()
            .and_then(|n| n.strip_prefix(&prefix))
            .filter(|digits| digits.len() == 20 && digits.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|digits| digits.parse::<u64>().ok())
        else {
            continue;
        };
        // Not followed: a segment is a file of this process's.
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.is_file() {
            out.push((start, meta.len()));
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// The bytes process `number` wrote in all, dropped ones included.
pub fn total(dir: &Path, number: u64) -> Result<u64, String> {
    Ok(segments(dir, number)?
        .last()
        .map_or(0, |(start, len)| start.saturating_add(*len)))
}

/// At most `max` bytes of process `number`'s output from offset `from`,
/// or from where what is retained begins when that is later; cut between
/// characters.
pub fn read(dir: &Path, number: u64, from: u64, max: usize) -> Result<Output, String> {
    let mut last = String::new();
    for _ in 0..READ_TRIES {
        match read_once(dir, number, from, max) {
            Ok(Some(output)) => return Ok(output),
            // A segment dropped between listing and reading it.
            Ok(None) => last = "its output was dropped while it was read".into(),
            Err(e) => return Err(e),
        }
    }
    Err(format!("p{number}: {last}"))
}

/// The last at most `max` bytes of process `number`'s output.
pub fn tail(dir: &Path, number: u64, max: usize) -> Result<Output, String> {
    let total = total(dir, number)?;
    read(dir, number, total.saturating_sub(max as u64), max)
}

fn read_once(dir: &Path, number: u64, from: u64, max: usize) -> Result<Option<Output>, String> {
    let segments = segments(dir, number)?;
    let (Some(&(start, _)), Some(&(last, last_len))) = (segments.first(), segments.last()) else {
        return Ok(Some(Output::default()));
    };
    let total = last.saturating_add(last_len);
    let at = from.clamp(start, total);
    // Room for a whole character, so a read always moves on.
    let want = usize::try_from(total.saturating_sub(at))
        .unwrap_or(usize::MAX)
        .min(max.max(4));
    let mut bytes = Vec::with_capacity(want);
    for (seg_start, seg_len) in &segments {
        let have = at.saturating_add(bytes.len() as u64);
        if bytes.len() >= want {
            break;
        }
        let seg_end = seg_start.saturating_add(*seg_len);
        if seg_end <= have || *seg_start > have {
            continue;
        }
        let path = dir.join(name(number, *seg_start));
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        file.seek(SeekFrom::Start(have.saturating_sub(*seg_start)))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let need = (want - bytes.len()) as u64;
        let take = need.min(seg_end.saturating_sub(have));
        let read = file
            .take(take)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if (read as u64) < take {
            break;
        }
    }
    // A start inside a character, after a dropped segment or a `from`
    // that falls there, moves on to the next whole one.
    let skip = bytes
        .iter()
        .take(3)
        .take_while(|b| **b & 0xC0 == 0x80)
        .count();
    let body = bytes.get(skip..).unwrap_or_default();
    let whole = match std::str::from_utf8(body) {
        Ok(_) => body.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => body.len(),
    };
    let body = body.get(..whole).unwrap_or_default();
    let from = at.saturating_add(skip as u64);
    Ok(Some(Output {
        text: String::from_utf8_lossy(body).into_owned(),
        from,
        to: from.saturating_add(body.len() as u64),
        start,
        total,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "td-agent-output-{tag}-{}-{}",
            std::process::id(),
            crate::store::random_hex(4).unwrap()
        ));
        dir.join(DIR)
    }

    #[test]
    fn output_is_kept_in_segments_and_the_oldest_dropped_past_the_cap() {
        let dir = scratch("cap");
        let mut writer = Writer::create(&dir, 3, 40).unwrap();
        assert_eq!(total(&dir, 3).unwrap(), 0);
        assert_eq!(read(&dir, 3, 0, 100).unwrap(), Output::default());
        writer.push(b"0123456789").unwrap();
        writer.push(b"abcdefghij").unwrap();
        // Read whole, from anywhere, and bounded.
        let all = read(&dir, 3, 0, 100).unwrap();
        assert_eq!(
            (all.text.as_str(), all.from, all.to),
            ("0123456789abcdefghij", 0, 20)
        );
        assert_eq!((all.start, all.total), (0, 20));
        let some = read(&dir, 3, 5, 7).unwrap();
        assert_eq!((some.text.as_str(), some.from, some.to), ("56789ab", 5, 12));
        // Past 40 bytes, the oldest 10-byte segments go.
        writer.push(&[b'x'; 35]).unwrap();
        assert_eq!(writer.total(), 55);
        let kept = read(&dir, 3, 0, 100).unwrap();
        assert_eq!((kept.start, kept.from, kept.total), (20, 20, 55));
        assert_eq!(kept.text, "x".repeat(35));
        assert_eq!(total(&dir, 3).unwrap(), 55);
        assert_eq!(tail(&dir, 3, 4).unwrap().text, "xxxx");
        // Another process's output is its own.
        assert_eq!(total(&dir, 30).unwrap(), 0);
        // A writer for a number with output left over starts afresh.
        let mut again = Writer::create(&dir, 3, 40).unwrap();
        assert_eq!(total(&dir, 3).unwrap(), 0);
        again.push(b"new").unwrap();
        assert_eq!(read(&dir, 3, 0, 100).unwrap().text, "new");
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_read_is_cut_between_characters() {
        let dir = scratch("utf8");
        let mut writer = Writer::create(&dir, 1, 1000).unwrap();
        writer.push("aé€😀".as_bytes()).unwrap();
        // From inside the euro sign: on to the next whole character.
        let read_at = |from: u64, max: usize| {
            let out = read(&dir, 1, from, max).unwrap();
            (out.text, out.from, out.to)
        };
        assert_eq!(read_at(4, 100), ("😀".to_string(), 6, 10));
        // A read that would end inside a character stops before it.
        assert_eq!(read_at(0, 5), ("aé".to_string(), 0, 3));
        // One too small for the next character takes it whole.
        assert_eq!(read_at(6, 1), ("😀".to_string(), 6, 10));
        assert_eq!(read_at(0, 100).0, "aé€😀");
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_link_is_neither_a_segment_nor_followed() {
        let dir = scratch("link");
        let mut writer = Writer::create(&dir, 2, 1000).unwrap();
        writer.push(b"own").unwrap();
        let elsewhere = dir.parent().unwrap().join("secret");
        fs::write(&elsewhere, "secret").unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join(name(2, 3))).unwrap();
        let out = read(&dir, 2, 0, 100).unwrap();
        assert_eq!((out.text.as_str(), out.total), ("own", 3));
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}
