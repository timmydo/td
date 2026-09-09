//! Bounded read-only snapshots. Display strings are never parsed as paths.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const ENTRIES: usize = 4096;
const NAME_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Snapshot {
    pub(crate) path: PathBuf,
    pub(crate) title: String,
    pub(crate) text: String,
    entries: Vec<Entry>,
    pub(crate) sort: Sort,
    pub(crate) reverse: bool,
    parent_identity: (u64, u64),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Sort {
    #[default]
    Name,
    Size,
    Modified,
}
impl Sort {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Size => "size",
            Self::Modified => "modified",
        }
    }
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Name => Self::Size,
            Self::Size => Self::Modified,
            Self::Modified => Self::Name,
        }
    }
}

#[derive(Clone)]
struct Entry {
    name: OsString,
    kind: char,
    mode: u32,
    links: u64,
    uid: u32,
    gid: u32,
    size: u64,
    modified: (i64, i64),
    stamp: crate::files::Stamp,
}

impl Snapshot {
    pub(crate) fn rename_source(&self, row: usize) -> Option<crate::files::RenameSource> {
        let entry = self.entries.get(row)?;
        Some(crate::files::RenameSource::observed(
            self.path.join(&entry.name),
            self.parent_identity,
            entry.stamp.clone(),
        ))
    }

    pub(crate) fn relocate(&mut self, path: PathBuf) {
        self.title = title(&path);
        self.path = path;
    }

    pub(crate) fn entry(&self, row: usize) -> Option<PathBuf> {
        self.entries
            .get(row)
            .map(|entry| self.path.join(&entry.name))
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn arrange(&mut self, sort: Sort, reverse: bool) {
        self.sort = sort;
        self.reverse = reverse;
        self.entries.sort_by(|a, b| {
            let primary = match sort {
                Sort::Name => std::cmp::Ordering::Equal,
                Sort::Size => b.size.cmp(&a.size),
                Sort::Modified => b.modified.cmp(&a.modified),
            }
            .then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()));
            (a.kind != 'd').cmp(&(b.kind != 'd')).then(if reverse {
                primary.reverse()
            } else {
                primary
            })
        });
        self.text.clear();
        for entry in &self.entries {
            if !self.text.is_empty() {
                self.text.push('\n');
            }
            self.text.push_str(&format!(
                "{} {:>3} {:>5} {:>5} {:>10} {} ",
                permissions(entry.kind, entry.mode),
                entry.links,
                entry.uid,
                entry.gid,
                entry.size,
                timestamp(entry.modified.0)
            ));
            // Display is escaped ASCII; actions retain literal OS bytes.
            for byte in entry.name.as_bytes() {
                for escaped in std::ascii::escape_default(*byte) {
                    self.text.push(char::from(escaped));
                }
            }
            if entry.kind == 'd' {
                self.text.push('/');
            }
        }
    }

    pub(crate) fn offset(&self, path: &Path) -> usize {
        if path.parent() != Some(self.path.as_path()) {
            return 0;
        }
        let Some(row) = self
            .entries
            .iter()
            .position(|entry| Some(entry.name.as_os_str()) == path.file_name())
        else {
            return 0;
        };
        self.text.lines().take(row).map(|line| line.len() + 1).sum()
    }
}

fn permissions(kind: char, mode: u32) -> String {
    let mut text = String::with_capacity(10);
    text.push(kind);
    for (shift, special, lower, upper) in [
        (6, 0o4000, 's', 'S'),
        (3, 0o2000, 's', 'S'),
        (0, 0o1000, 't', 'T'),
    ] {
        let bits = mode >> shift;
        text.push(if bits & 4 != 0 { 'r' } else { '-' });
        text.push(if bits & 2 != 0 { 'w' } else { '-' });
        text.push(match (bits & 1 != 0, mode & special != 0) {
            (true, true) => lower,
            (false, true) => upper,
            (true, false) => 'x',
            (false, false) => '-',
        });
    }
    text
}

// Gregorian civil conversion, also used in td-news/civil.rs. Dividing seconds
// first bounds every intermediate even for the full signed timestamp range.
fn timestamp(seconds: i64) -> String {
    let z = seconds.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    if !(0..=9999).contains(&year) {
        return "????-??-?? ??:??Z".into();
    }
    let time = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}Z",
        time / 3600,
        time / 60 % 60
    )
}

/// None delegates to the existing regular/missing-file adapter. Final symlinks
/// are not followed, including when the caller supplied a trailing slash.
pub(crate) fn read(path: &Path) -> Result<Option<Snapshot>, String> {
    let raw = path.as_os_str().as_bytes();
    if raw.is_empty() || raw.len() > 4096 || raw.contains(&0) {
        return Err("Path must contain 1..=4096 non-NUL bytes".into());
    }
    let trimmed = raw.strip_suffix(b"/").map_or(raw, |_| {
        let end = raw
            .iter()
            .rposition(|byte| *byte != b'/')
            .map_or(1, |i| i + 1);
        raw.get(..end).unwrap_or(raw)
    });
    let path = Path::new(std::ffi::OsStr::from_bytes(trimmed));
    let before = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => meta,
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Cannot inspect directory: {e}")),
    };
    let path = std::fs::canonicalize(path).map_err(|e| format!("Cannot resolve directory: {e}"))?;
    if path.as_os_str().as_bytes().len() > 4096 {
        return Err("Resolved directory path exceeds 4096 bytes".into());
    }
    let mut entries = Vec::new();
    let mut names = 0usize;
    for entry in std::fs::read_dir(&path).map_err(|e| format!("Cannot list directory: {e}"))? {
        let entry = entry.map_err(|e| format!("Cannot read directory entry: {e}"))?;
        let name = entry.file_name();
        names = names
            .checked_add(name.as_bytes().len())
            .ok_or("Directory name budget exhausted")?;
        if entries.len() == ENTRIES || names > NAME_BYTES {
            return Err(
                "Directory exceeds 4096 entries or 1 MiB of names; no partial listing admitted"
                    .into(),
            );
        }
        let metadata = std::fs::symlink_metadata(entry.path())
            .map_err(|e| format!("Cannot inspect directory entry: {e}"))?;
        let kind = metadata.file_type();
        entries.push(Entry {
            name,
            kind: if kind.is_dir() {
                'd'
            } else if kind.is_file() {
                '-'
            } else if kind.is_symlink() {
                'l'
            } else if kind.is_block_device() {
                'b'
            } else if kind.is_char_device() {
                'c'
            } else if kind.is_fifo() {
                'p'
            } else if kind.is_socket() {
                's'
            } else {
                '?'
            },
            mode: metadata.mode(),
            links: metadata.nlink(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            size: metadata.size(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            stamp: crate::files::Stamp::read(&metadata),
        });
    }
    let after = std::fs::symlink_metadata(&path).map_err(|e| format!("Directory changed: {e}"))?;
    if !after.is_dir() || (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        return Err("Directory replaced during listing; retry Open".into());
    }
    let title = title(&path);
    let mut snapshot = Snapshot {
        path,
        title,
        text: String::new(),
        entries,
        sort: Sort::Name,
        reverse: false,
        parent_identity: (before.dev(), before.ino()),
    };
    snapshot.arrange(Sort::Name, false);
    Ok(Some(snapshot))
}

fn title(path: &Path) -> String {
    format!("[dir] {:?}", path.file_name().unwrap_or(path.as_os_str()))
        .chars()
        .take(80)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_special_bits_and_signed_utc_dates_are_exact() {
        assert_eq!(permissions('-', 0o4755), "-rwsr-xr-x");
        assert_eq!(permissions('d', 0o3770), "drwxrws--T");
        assert_eq!(permissions('-', 0o7000), "---S--S--T");
        assert_eq!(permissions('l', 0o777), "lrwxrwxrwx");
        assert_eq!(timestamp(0), "1970-01-01 00:00Z");
        assert_eq!(timestamp(-1), "1969-12-31 23:59Z");
        assert_eq!(timestamp(951_782_400), "2000-02-29 00:00Z");
        assert_eq!(timestamp(4_107_542_400), "2100-03-01 00:00Z");
        assert_eq!(timestamp(i64::MIN), "????-??-?? ??:??Z");
        assert_eq!(timestamp(i64::MAX), "????-??-?? ??:??Z");
    }

    #[test]
    fn metadata_rows_sort_without_parsing_names_and_keep_directories_first() {
        let stamp = crate::files::Stamp::read(&std::fs::metadata(file!()).unwrap());
        let entry = |name: &[u8], kind, size, modified| Entry {
            name: std::ffi::OsStr::from_bytes(name).to_owned(),
            kind,
            mode: 0o640,
            links: 2,
            uid: 12,
            gid: 34,
            size,
            modified,
            stamp: stamp.clone(),
        };
        let mut snapshot = Snapshot {
            path: PathBuf::from("/browse"),
            title: String::new(),
            text: String::new(),
            sort: Sort::Name,
            reverse: false,
            parent_identity: (0, 0),
            entries: vec![
                entry(b"z\xff\n", '-', 99, (0, 1)),
                entry(b"a", '-', 1, (0, 2)),
                entry(b"folder", 'd', 0, (-1, 0)),
                entry(b"same", '-', 99, (0, 1)),
            ],
        };
        snapshot.arrange(Sort::Name, false);
        assert_eq!(snapshot.entry(0), Some(PathBuf::from("/browse/folder")));
        assert_eq!(snapshot.entry(1), Some(PathBuf::from("/browse/a")));
        assert!(snapshot
            .text
            .lines()
            .nth(1)
            .unwrap()
            .contains("-rw-r-----   2    12    34          1 1970-01-01 00:00Z a"));
        assert!(snapshot.text.ends_with(" z\\xff\\n"));
        snapshot.arrange(Sort::Size, false);
        assert_eq!(snapshot.entry(1), Some(PathBuf::from("/browse/same")));
        assert_eq!(snapshot.entry(3), Some(PathBuf::from("/browse/a")));
        snapshot.arrange(Sort::Modified, false);
        assert_eq!(snapshot.entry(1), Some(PathBuf::from("/browse/a")));
        snapshot.arrange(Sort::Modified, true);
        assert_eq!(snapshot.entry(0), Some(PathBuf::from("/browse/folder")));
        assert_eq!(snapshot.entry(3), Some(PathBuf::from("/browse/a")));
        let offset = snapshot.offset(Path::new("/browse/a"));
        assert!(snapshot.text.get(offset..).unwrap().ends_with(" a"));
        assert_eq!(snapshot.text.lines().count(), 4);
    }
}
