//! Bounded read-only snapshots. Display strings are never parsed as paths.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const ENTRIES: usize = 4096;
const NAME_BYTES: usize = 1024 * 1024;

pub(crate) struct Snapshot {
    pub(crate) path: PathBuf,
    pub(crate) title: String,
    pub(crate) text: String,
    entries: Vec<Entry>,
}

struct Entry {
    name: OsString,
    kind: char,
}

impl Snapshot {
    pub(crate) fn entry(&self, row: usize) -> Option<PathBuf> {
        self.entries
            .get(row)
            .map(|entry| self.path.join(&entry.name))
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
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
        let kind = entry
            .file_type()
            .map_err(|e| format!("Cannot inspect directory entry: {e}"))?;
        entries.push(Entry {
            name,
            kind: if kind.is_dir() {
                'd'
            } else if kind.is_file() {
                'f'
            } else if kind.is_symlink() {
                'l'
            } else {
                '?'
            },
        });
    }
    let after = std::fs::symlink_metadata(&path).map_err(|e| format!("Directory changed: {e}"))?;
    if !after.is_dir() || (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        return Err("Directory replaced during listing; retry Open".into());
    }
    entries.sort_by(|a, b| {
        (a.kind != 'd', a.name.as_bytes()).cmp(&(b.kind != 'd', b.name.as_bytes()))
    });
    let mut text = String::new();
    for entry in &entries {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push(entry.kind);
        text.push(' ');
        // ASCII escapes are unambiguous, including literal backslashes, control
        // bytes and non-UTF-8 names. Activation uses the retained OsString.
        for byte in entry.name.as_bytes() {
            for escaped in std::ascii::escape_default(*byte) {
                text.push(char::from(escaped));
            }
        }
        if entry.kind == 'd' {
            text.push('/');
        }
    }
    let title = format!("[dir] {:?}", path.file_name().unwrap_or(path.as_os_str()))
        .chars()
        .take(80)
        .collect();
    Ok(Some(Snapshot {
        path,
        title,
        text,
        entries,
    }))
}
