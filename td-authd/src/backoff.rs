//! The public elevation intakes' backoff (td-authd/DESIGN.md, "Elevation
//! operations", "Backoff"): each intake's count of admitted requests not
//! yet approved and the wall-clock second before which it refuses another,
//! in one root-owned file on `@var` that neither a new authority
//! generation nor a reboot clears. Only an approval clears it.

use crate::saved;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DIRECTORY: &str = "/var/lib/td/authd";
const FILE: &str = "backoff";
const HEADER: &str = "td-authd-backoff-v1";
/// The hostname intake's row; L5 adds the update queue's.
const HOSTNAME: &str = "hostname";
const LIMIT: u64 = 256;
const NOFOLLOW: i32 = 0x20000;
const NONBLOCK: i32 = 0x800;
const DIRECTORY_ONLY: i32 = 0x10000;
/// The first refusal, in seconds, doubling with each consecutive
/// unapproved request to the last.
const FIRST: u64 = 30;
const LAST: u64 = 960;
/// The longest an admitted request lives: its 60-second selection window,
/// then its 120-second consent window. Its refusal runs past that.
const LIFETIME: u64 = 180;

/// One intake's consecutive unapproved requests and the second, since the
/// epoch, before which it refuses another.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Entry {
    count: u32,
    until: u64,
}

impl Entry {
    pub fn count(self) -> u32 {
        self.count
    }

    /// Whether a request arriving at `now` is refused.
    pub fn refuses(self, now: u64) -> bool {
        now < self.until
    }

    /// The refusal cut to the longest one admission at `now` could cause,
    /// so a clock set back never refuses past that bound.
    pub fn clamped(self, now: u64) -> Self {
        Self {
            count: self.count,
            until: self
                .until
                .min(now.saturating_add(LIFETIME).saturating_add(LAST)),
        }
    }

    /// One more unapproved request, ending at `now` plus `window`: the
    /// refusal then runs its doubled delay past that.
    fn after(self, now: u64, window: u64) -> Self {
        let count = self.count.saturating_add(1);
        let doublings = count.saturating_sub(1).min(5);
        let delay = FIRST.checked_shl(doublings).unwrap_or(LAST).min(LAST);
        Self {
            count,
            until: now.saturating_add(window).saturating_add(delay),
        }
    }
}

/// The current second since the epoch; a clock before it reads zero.
pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The backoff file's directory and the owner it and the file must have.
#[derive(Clone, Debug)]
pub(crate) struct Backoff {
    directory: PathBuf,
    owner: (u32, u32),
}

impl Backoff {
    /// `/var/lib/td/authd/backoff`, root's.
    pub fn system() -> Self {
        Self::at(Path::new(DIRECTORY), (0, 0))
    }

    pub(crate) fn at(directory: &Path, owner: (u32, u32)) -> Self {
        Self {
            directory: directory.to_path_buf(),
            owner,
        }
    }

    /// The hostname intake's entry: zero while the directory or the file
    /// is missing. The directory must be the owner's, mode 0700, and the
    /// file one link of the owner's, mode 0600, within its bound, read
    /// without following a link or waiting on a FIFO, in the exact grammar.
    pub fn read(&self) -> Result<Entry, String> {
        let directory = match self.directory() {
            Ok(directory) => directory,
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => return Ok(Entry::default()),
            Err(why) => return Err(format!("open the backoff directory: {why}")),
        };
        self.admit_directory(&directory)?;
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(format!("/proc/self/fd/{}/{FILE}", directory.as_raw_fd()))
        {
            Ok(file) => file,
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => return Ok(Entry::default()),
            Err(why) => return Err(format!("open the backoff file: {why}")),
        };
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file()
            || (metadata.uid(), metadata.gid()) != self.owner
            || metadata.mode() & 0o7777 != 0o600
            || metadata.nlink() != 1
            || metadata.len() > LIMIT
        {
            return Err("the backoff file is not one bounded mode-0600 file of root's".into());
        }
        let mut text = String::new();
        file.take(LIMIT + 1)
            .read_to_string(&mut text)
            .map_err(|e| format!("read the backoff file: {e}"))?;
        parse(&text)
    }

    /// `entry`, as read at `now`, clamped; a cut refusal is written back,
    /// so it ends at its bound rather than moving with the clock.
    pub fn clamp(&self, entry: Entry, now: u64) -> Result<Entry, String> {
        let clamped = entry.clamped(now);
        if clamped == entry {
            Ok(entry)
        } else {
            self.record(clamped)
        }
    }

    /// At admission, before the requester hears of it: one more request
    /// not yet approved, refused past the longest it can live. So every
    /// admitted request counts, however it later ends, until an approval.
    pub fn admitted(&self, entry: Entry, now: u64) -> Result<Entry, String> {
        self.record(entry.after(now, LIFETIME))
    }

    /// An approval clears the count and the refusal.
    pub fn approved(&self) -> Result<Entry, String> {
        self.record(Entry::default())
    }

    fn directory(&self) -> std::io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW | DIRECTORY_ONLY)
            .open(&self.directory)
    }

    fn admit_directory(&self, directory: &File) -> Result<(), String> {
        let metadata = directory.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_dir()
            || (metadata.uid(), metadata.gid()) != self.owner
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err("the backoff directory is not root's, mode 0700".into());
        }
        Ok(())
    }

    /// Writes `entry`, synced, creating the directory, mode 0700, beneath
    /// a parent of the owner's that no other may write.
    fn record(&self, entry: Entry) -> Result<Entry, String> {
        let directory = match self.directory() {
            Ok(directory) => directory,
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => self.create()?,
            Err(why) => return Err(format!("open the backoff directory: {why}")),
        };
        self.admit_directory(&directory)?;
        saved::write_synced(&self.directory.join(FILE), &encode(entry), 0o600, None)?;
        Ok(entry)
    }

    fn create(&self) -> Result<File, String> {
        let parent = self
            .directory
            .parent()
            .ok_or("the backoff directory has no parent")?;
        let metadata = std::fs::symlink_metadata(parent).map_err(|e| e.to_string())?;
        if !metadata.is_dir() || metadata.uid() != self.owner.0 || metadata.mode() & 0o022 != 0 {
            return Err("the backoff directory's parent is not protected".into());
        }
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&self.directory)
            .map_err(|e| format!("create the backoff directory: {e}"))?;
        std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("protect the backoff directory: {e}"))?;
        self.directory()
            .map_err(|e| format!("open the backoff directory: {e}"))
    }
}

/// `td-authd-backoff-v1`, then the hostname intake's row when its count is
/// not zero: `hostname`, the count and the second, each after a tab, in
/// canonical decimal, every line newline-terminated.
fn parse(text: &str) -> Result<Entry, String> {
    let invalid = || "the backoff file is malformed".to_string();
    let body = text.strip_suffix('\n').ok_or_else(invalid)?;
    let mut lines = body.split('\n');
    if lines.next() != Some(HEADER) {
        return Err(invalid());
    }
    let entry = match lines.next() {
        None => Entry::default(),
        Some(row) => {
            let mut fields = row.split('\t');
            if fields.next() != Some(HOSTNAME) {
                return Err(invalid());
            }
            let count = fields.next().and_then(decimal).ok_or_else(invalid)?;
            let until = fields.next().and_then(decimal).ok_or_else(invalid)?;
            if fields.next().is_some() || count == 0 {
                return Err(invalid());
            }
            Entry {
                count: u32::try_from(count).map_err(|_| invalid())?,
                until,
            }
        }
    };
    if lines.next().is_some() {
        return Err(invalid());
    }
    Ok(entry)
}

fn decimal(text: &str) -> Option<u64> {
    if text.is_empty()
        || (text.len() > 1 && text.starts_with('0'))
        || !text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    text.parse().ok()
}

fn encode(entry: Entry) -> Vec<u8> {
    let mut text = format!("{HEADER}\n");
    if entry.count != 0 {
        text.push_str(&format!("{HOSTNAME}\t{}\t{}\n", entry.count, entry.until));
    }
    text.into_bytes()
}

#[cfg(test)]
#[path = "../tests/backoff.rs"]
pub(crate) mod tests;
