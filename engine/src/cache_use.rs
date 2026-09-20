//! When a cache record was last USED, and how a use says so — the one rule
//! td-builder (for the build cache's receipts) and td-recipe-eval (for its
//! memos) share, since `gc-store` judges what both of them wrote.
//!
//! The warm reuse paths only ever read their record, so the only trace a hit
//! leaves on its own is the atime a `relatime` mount refreshes once a day at
//! best and a `noatime` one never. [`stamp`] bumps the record's mtime at the
//! hit, and [`last_used`] reads the newer of mtime and atime, so a record can
//! only be judged used MORE recently than the stamp says, never less: a record
//! written before the stamp existed still counts its last read, and a stray
//! `grep` over the cache keeps an entry a little longer rather than losing one.
//! Judging never reads the record — that would be a use — and a reclaim that
//! must read one reads it through [`read_to_string_leaving_no_use`], which puts
//! the atime back, so a reclaim run every day cannot keep a record fresh
//! forever by its own reading.

use std::fs::{self, File, FileTimes};
use std::io;
use std::path::Path;
use std::time::SystemTime;

/// Record that the cache record at PATH was just used: bump its mtime to now,
/// leaving the bytes alone. Best-effort, since a failed stamp must never fail
/// the hit that made it: an absent or unwritable record is simply not stamped.
pub fn stamp(path: &Path) {
    if let Ok(f) = File::open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

/// When the record at PATH was last used: the newer of its mtime (the stamp, or
/// the write that created it) and its atime (a reader's trace). Never reads the
/// file. A symlink is judged by itself, not by what it points at.
pub fn last_used(path: &Path) -> io::Result<SystemTime> {
    let md = fs::symlink_metadata(path)?;
    let m = md.modified()?;
    let a = md.accessed()?;
    Ok(if a > m { a } else { m })
}

/// Read the record at PATH without leaving a use: its atime is put back to what
/// it was before the read (mtime is untouched — the other field is omitted from
/// the update), whether or not the read succeeded, since a read that fails
/// partway — or reads bytes that are not UTF-8 — has already left its trace.
/// The restore is best-effort; a failed one leaves the atime the read set, so a
/// record whose atime cannot be put back (not ours to set) is over-retained for
/// as long as that stays true — the safe direction. PATH is a regular file: the
/// atime taken is the link's own, while the read follows it.
pub fn read_to_string_leaving_no_use(path: &Path) -> io::Result<String> {
    let before = fs::symlink_metadata(path)?.accessed()?;
    let bytes = fs::read(path);
    if let Ok(f) = File::open(path) {
        let _ = f.set_times(FileTimes::new().set_accessed(before));
    }
    String::from_utf8(bytes?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn set_times(path: &Path, accessed: SystemTime, modified: SystemTime) {
        File::open(path)
            .and_then(|f| f.set_times(FileTimes::new().set_accessed(accessed).set_modified(modified)))
            .unwrap();
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("td-cache-use-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn last_used_is_the_newer_timestamp_and_the_stamp_moves_only_mtime() {
        let d = scratch("stamp");
        let f = d.join("r.receipt");
        fs::write(&f, "td-receipt v1\n").unwrap();
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let t1 = t0 + Duration::from_secs(3600);
        set_times(&f, t0, t1);
        assert_eq!(last_used(&f).ok(), Some(t1), "mtime newer");
        set_times(&f, t1, t0);
        assert_eq!(last_used(&f).ok(), Some(t1), "atime newer");
        stamp(&f);
        assert!(last_used(&f).ok() > Some(t1), "the stamp is now");
        assert_eq!(
            fs::symlink_metadata(&f).and_then(|m| m.accessed()).ok(),
            Some(t1),
            "the stamp leaves atime alone"
        );
        assert_eq!(fs::read_to_string(&f).ok().as_deref(), Some("td-receipt v1\n"), "bytes untouched");
        assert!(last_used(&d.join("absent")).is_err());
        stamp(&d.join("absent"));
        assert!(!d.join("absent").exists(), "a stamp creates nothing");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_read_that_leaves_no_use_puts_the_atime_back() {
        let d = scratch("noatime");
        let f = d.join("x.map");
        fs::write(&f, "fingerprint a\ngcc xxx-gcc\n").unwrap();
        // Old enough that even relatime would refresh the atime on a plain read.
        let old = SystemTime::now() - Duration::from_secs(30 * 86_400);
        set_times(&f, old, old);
        assert_eq!(
            read_to_string_leaving_no_use(&f).ok().as_deref(),
            Some("fingerprint a\ngcc xxx-gcc\n")
        );
        assert_eq!(last_used(&f).ok(), Some(old), "reading it was not a use");
        // Bytes that are not UTF-8 are an error, and still not a use.
        fs::write(&f, b"fingerprint a\ngcc \xff\xfe-gcc\n").unwrap();
        set_times(&f, old, old);
        let err = read_to_string_leaving_no_use(&f).err().map(|e| e.kind());
        assert_eq!(err, Some(io::ErrorKind::InvalidData));
        assert_eq!(last_used(&f).ok(), Some(old), "a failed read was not a use either");
        assert!(read_to_string_leaving_no_use(&d.join("absent")).is_err());
        let _ = fs::remove_dir_all(&d);
    }
}
