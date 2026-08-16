//! Opening and reading a file that must be a REAL, BOUNDED regular file.
//!
//! Shared for `protocol.rs`'s reason: td-boot verifies, `td-install` carries
//! the trust root onto the volume, and `td-net` signs — all three must refuse
//! the same files, or a bundle that signs is one that fails at boot.
//!
//! Each rule answers something that cannot be observed after the fact:
//!
//! - The type is settled by `lstat` BEFORE the open, because opening a FIFO
//!   read-only BLOCKS until a writer appears — a check that runs afterwards
//!   never runs at all, and the caller hangs with no diagnostic.
//! - The device/inode pair is compared ACROSS the open, because the entry
//!   can be replaced in between: a symlink put there after the `lstat` is
//!   opened and followed, and the target is a regular file too.
//! - The read takes one byte PAST the limit and refuses if it gets it, because
//!   a file that grew after the stat would otherwise be cut to exactly the
//!   limit and carried on as if it had always been that size.
//!
//! Std-only and importing nothing from any of them, as `fixture.rs` is, so
//! including it cannot drag anything into the crate that includes it.

use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::path::Path;

fn refused(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Same inode on the same device — the PAIR, because an inode number alone
/// repeats across filesystems and a device alone says nothing about which file.
///
/// A named function so the comparison is testable. The race it guards needs
/// interposition to reproduce, but a comparison that dropped either half would
/// be silently wrong, and that a test can catch. `std::os::unix` rather than
/// `std::os::linux`: `dev`/`ino` are the same two fields as `st_dev`/`st_ino`.
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Open `path`, requiring it to be a regular file that did not change into
/// something else while it was being opened. `label` names it in diagnostics.
///
/// No per-function `#[allow(dead_code)]`: every crate that uses only one of
/// the two puts the allow on the `mod`, so leaving it off here is what makes
/// one going unused in td-boot — which uses both — a warning rather than
/// silence.
pub fn open_real_file(path: &Path, label: &str) -> io::Result<(File, Metadata)> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(error.kind(), format!("{label} {}: {error}", path.display()))
    })?;
    if !metadata.file_type().is_file() {
        return Err(refused(format!(
            "{label} must be a real regular file: {}",
            path.display()
        )));
    }
    let file = File::open(path).map_err(|error| {
        io::Error::new(error.kind(), format!("{label} {}: {error}", path.display()))
    })?;
    let opened = file.metadata().map_err(|error| {
        io::Error::new(error.kind(), format!("{label} {}: {error}", path.display()))
    })?;
    if !opened.file_type().is_file() || !same_file(&metadata, &opened) {
        return Err(refused(format!(
            "{label} changed while opening: {}",
            path.display()
        )));
    }
    Ok((file, opened))
}

/// `open_real_file` plus a bound of `limit` bytes on what is read.
pub fn read_bounded_real_file(path: &Path, label: &str, limit: u64) -> io::Result<Vec<u8>> {
    let (file, metadata) = open_real_file(path, label)?;
    if metadata.len() > limit {
        return Err(refused(format!(
            "{label} exceeds {limit} bytes: {}",
            path.display()
        )));
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            io::Error::new(error.kind(), format!("{label} {}: {error}", path.display()))
        })?;
    if bytes.len() as u64 > limit {
        return Err(refused(format!(
            "{label} changed while reading or exceeds {limit} bytes: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

// Tested HERE, at the rule, rather than only through each caller: this file is
// `#[path]`-included by three crates, so these run in each of them and prove
// the rule holds in every inclusion context. The cases are the three the
// td-install copy got wrong before it was deleted, which is why they are the
// ones written down.
#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: u64 = 96;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "td-realfile-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_symlink_is_refused_even_when_its_target_is_a_regular_file() {
        let dir = scratch("symlink");
        let target = dir.join("real");
        std::fs::write(&target, b"key\n").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let refused = read_bounded_real_file(&link, "key", LIMIT)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("must be a real regular file"), "{refused}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The type is settled BEFORE the open, which is the only order that
    /// works: `File::open` on a FIFO blocks until a writer appears, so a
    /// version that opened first does not fail this test — it HANGS it.
    #[test]
    fn a_fifo_is_refused_rather_than_blocking_forever() {
        let dir = scratch("fifo");
        let fifo = dir.join("fifo");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.map(|status| status.success()).unwrap_or(false) {
            let refused = read_bounded_real_file(&fifo, "key", LIMIT)
                .unwrap_err()
                .to_string();
            assert!(refused.contains("must be a real regular file"), "{refused}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The bound is on what is READ and not only on what `stat` claimed. A
    /// file whose reported length disagrees with its contents is the case, and
    /// `/proc/self/status` is one without a race to arrange.
    #[test]
    fn a_file_longer_than_it_claims_is_refused_rather_than_truncated() {
        let path = std::path::Path::new("/proc/self/status");
        assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
        let refused = read_bounded_real_file(path, "key", LIMIT)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("changed while reading"), "{refused}");
    }

    #[test]
    fn a_file_at_the_limit_is_read_and_one_over_it_is_refused() {
        let dir = scratch("bound");
        let at = dir.join("at");
        std::fs::write(&at, vec![b'a'; LIMIT as usize]).unwrap();
        assert_eq!(
            read_bounded_real_file(&at, "key", LIMIT).unwrap().len(),
            LIMIT as usize
        );
        let over = dir.join("over");
        std::fs::write(&over, vec![b'a'; LIMIT as usize + 1]).unwrap();
        let refused = read_bounded_real_file(&over, "key", LIMIT)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("key exceeds 96 bytes"), "{refused}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The identity pin is the one rule here whose failure is SILENT — a
    /// comparison missing either half accepts a different file and says
    /// nothing — so the comparison is a named function and this is its test.
    /// The race itself needs interposition and is not reproduced.
    #[test]
    fn file_identity_needs_the_device_and_the_inode() {
        let dir = scratch("ident");
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let (ma, mb) = (
            std::fs::symlink_metadata(&a).unwrap(),
            std::fs::symlink_metadata(&b).unwrap(),
        );
        assert!(same_file(&ma, &ma), "a file is itself");
        assert!(!same_file(&ma, &mb), "two files are not the same file");
        use std::os::unix::fs::MetadataExt;
        assert_ne!(
            ma.ino(),
            mb.ino(),
            "the fixture is only meaningful if the inodes differ"
        );
        assert_eq!(ma.dev(), mb.dev(), "...and the devices do not");
        let reopened = File::open(&a).unwrap().metadata().unwrap();
        assert!(
            same_file(&ma, &reopened),
            "the same path reopened is the same file"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_is_refused_and_a_missing_file_names_itself() {
        let dir = scratch("other");
        let refused = read_bounded_real_file(&dir, "key", LIMIT)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("must be a real regular file"), "{refused}");
        let missing = dir.join("nope");
        let refused = read_bounded_real_file(&missing, "key", LIMIT)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("nope"),
            "a refusal must name the path: {refused}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
