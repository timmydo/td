//! `td-boot admit RUN`: the deployment initramfs moves the selector's
//! admission record from its RAM-backed root to the system's `/run`
//! (td-install/ENCRYPTION.md "Verified account handoff"). The record never
//! reaches a disk. The member is inspected as `mount-root` inspects the key
//! member and removed on every path; a failure writes nothing and refuses
//! no boot, since a lost record costs a login, not a boot.
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use crate::{admission, invalid, protocol};

/// x86-64 Linux O_NOFOLLOW | O_NONBLOCK, as the key member is opened.
const OPEN_FLAGS: i32 = 0o400000 | 0o4000;
/// x86-64 Linux O_NOFOLLOW, for the record `admit` creates.
const NOFOLLOW: i32 = 0o400000;
/// The member's mode, as td-kexec writes it.
const MEMBER_MODE: u32 = 0o400;
const DIRECTORY_MODE: u32 = 0o700;
const RECORD_MODE: u32 = 0o600;

/// The owner the member, the directory and the record must have: root:root
/// in production, the test's own IDs in tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Owner {
    pub uid: u32,
    pub gid: u32,
}

impl Owner {
    pub const ROOT: Self = Self { uid: 0, gid: 0 };

    fn owns(self, meta: &fs::Metadata) -> bool {
        meta.uid() == self.uid && meta.gid() == self.gid
    }
}

fn refused(path: &Path, why: &str) -> io::Error {
    invalid(format!("admission record {}: {why}", path.display()))
}

fn named(path: &Path, what: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{what} {}: {error}", path.display()))
}

/// The member at `path`: a regular file, not a symlink, of mode 0400 owned
/// by `owner`'s UID, holding exactly the record's bytes. Absent is `None`.
fn read_member(path: &Path, owner: Owner) -> io::Result<Option<[u8; admission::RECORD_BYTES]>> {
    let before = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        other => other.map_err(|error| named(path, "inspect admission record", error))?,
    };
    if !before.file_type().is_file() {
        return Err(refused(path, "not a regular file"));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_FLAGS)
        .open(path)
        .map_err(|error| named(path, "open admission record", error))?;
    let meta = file
        .metadata()
        .map_err(|error| named(path, "stat admission record", error))?;
    if !meta.file_type().is_file() || meta.dev() != before.dev() || meta.ino() != before.ino() {
        return Err(refused(path, "changed while it was opened"));
    }
    if meta.mode() & 0o7777 != MEMBER_MODE {
        return Err(refused(
            path,
            &format!("mode {:04o}, not {MEMBER_MODE:04o}", meta.mode() & 0o7777),
        ));
    }
    if meta.uid() != owner.uid {
        return Err(refused(path, &format!("owned by uid {}", meta.uid())));
    }
    if meta.len() != protocol::ADMIT_RECORD_BYTES as u64 {
        return Err(refused(
            path,
            &format!("{} bytes, not {}", meta.len(), protocol::ADMIT_RECORD_BYTES),
        ));
    }
    let mut record = [0u8; admission::RECORD_BYTES];
    let mut reader = &file;
    reader
        .read_exact(&mut record)
        .map_err(|error| named(path, "read admission record", error))?;
    let mut more = [0u8; 1];
    if reader
        .read(&mut more)
        .map_err(|error| named(path, "read admission record", error))?
        != 0
    {
        return Err(refused(path, "longer than the record"));
    }
    Ok(Some(record))
}

/// Creates `run/td-admit` fresh, root:root mode 0700, and in it `v1`
/// exclusively, single-link root:root mode 0600, holding `record`. Anything
/// it created is removed again on a failure, so a failure writes nothing.
fn write_record(run: &Path, owner: Owner, record: &[u8]) -> io::Result<()> {
    let directory = run.join(admission::DIRECTORY);
    DirBuilder::new()
        .mode(DIRECTORY_MODE)
        .create(&directory)
        .map_err(|error| named(&directory, "create admission directory", error))?;
    let written = (|| {
        let meta = fs::symlink_metadata(&directory)
            .map_err(|error| named(&directory, "inspect admission directory", error))?;
        if !meta.is_dir() || meta.mode() & 0o7777 != DIRECTORY_MODE || !owner.owns(&meta) {
            return Err(refused(&directory, "the directory is not root's mode 0700"));
        }
        let path = directory.join(admission::RECORD);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(RECORD_MODE)
            .custom_flags(NOFOLLOW)
            .open(&path)
            .map_err(|error| named(&path, "create admission record", error))?;
        let result = (|| {
            let mut writer: &File = &file;
            writer.write_all(record)?;
            let meta = file.metadata()?;
            if !meta.is_file()
                || meta.nlink() != 1
                || meta.mode() & 0o7777 != RECORD_MODE
                || !owner.owns(&meta)
                || meta.len() != record.len() as u64
            {
                return Err(refused(&path, "not a single-link root file of mode 0600"));
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&path);
        }
        result
    })();
    if written.is_err() {
        let _ = fs::remove_dir(&directory);
    }
    written
}

/// Takes the member from the initramfs root `root` and writes it under the
/// system's `run`: reads it, removes it whether or not it passed, and only
/// then writes it. Returns whether a record was written; no member is
/// `false`. A refused member, a failed removal or a failed write writes
/// nothing and is an error the deployment init ignores.
pub(crate) fn admit(root: &Path, run: &Path, owner: Owner) -> io::Result<bool> {
    let member = root.join(protocol::ADMIT_MEMBER);
    let record = read_member(&member, owner);
    let removed = match fs::remove_file(&member) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other.map_err(|error| named(&member, "remove", error)),
    };
    let record = record?;
    removed?;
    match record {
        None => Ok(false),
        Some(record) => write_record(run, owner, &record).map(|()| true),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture {
        dir: std::path::PathBuf,
        owner: Owner,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "td-boot-admit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(dir.join("root")).unwrap();
            fs::create_dir_all(dir.join("run")).unwrap();
            let meta = fs::metadata(&dir).unwrap();
            Self {
                dir,
                owner: Owner {
                    uid: meta.uid(),
                    gid: meta.gid(),
                },
            }
        }

        fn root(&self) -> std::path::PathBuf {
            self.dir.join("root")
        }

        fn run(&self) -> std::path::PathBuf {
            self.dir.join("run")
        }

        fn member(&self) -> std::path::PathBuf {
            self.root().join(protocol::ADMIT_MEMBER)
        }

        fn put(&self, bytes: &[u8], mode: u32) {
            fs::write(self.member(), bytes).unwrap();
            fs::set_permissions(self.member(), fs::Permissions::from_mode(mode)).unwrap();
        }

        fn admit(&self) -> io::Result<bool> {
            admit(&self.root(), &self.run(), self.owner)
        }

        fn written(&self) -> Option<Vec<u8>> {
            fs::read(self.run().join("td-admit/v1")).ok()
        }

        /// Nothing under the system's /run, and the member gone.
        fn nothing_written(&self) {
            assert!(!self.run().join("td-admit").exists());
            assert!(fs::symlink_metadata(self.member()).is_err());
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::set_permissions(self.run(), fs::Permissions::from_mode(0o755));
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn record() -> [u8; 46] {
        admission::Record {
            uid: 1000,
            method: admission::Method::TpmPin,
            manifest: [0x5a; 32],
        }
        .encode()
    }

    #[test]
    fn the_member_moves_to_a_fresh_root_directory_and_leaves_the_initramfs() {
        let fixture = Fixture::new();
        fixture.put(&record(), 0o400);
        assert!(fixture.admit().unwrap());
        assert_eq!(fixture.written().unwrap(), record());
        let directory = fs::symlink_metadata(fixture.run().join("td-admit")).unwrap();
        assert!(directory.is_dir());
        assert_eq!(directory.mode() & 0o7777, 0o700);
        let file = fs::symlink_metadata(fixture.run().join("td-admit/v1")).unwrap();
        assert!(file.is_file());
        assert_eq!(file.mode() & 0o7777, 0o600);
        assert_eq!(file.nlink(), 1);
        assert!(fs::symlink_metadata(fixture.member()).is_err());
    }

    #[test]
    fn no_member_writes_nothing() {
        let fixture = Fixture::new();
        assert!(!fixture.admit().unwrap());
        fixture.nothing_written();
    }

    /// Every refusal of the member removes it and writes nothing.
    #[test]
    fn a_refused_member_is_removed_and_nothing_is_written() {
        for (bytes, mode) in [
            (record().to_vec(), 0o600),
            (record().to_vec(), 0o440),
            (record()[..45].to_vec(), 0o400),
            ([&record()[..], &[0]].concat(), 0o400),
            (Vec::new(), 0o400),
        ] {
            let fixture = Fixture::new();
            fixture.put(&bytes, mode);
            assert!(fixture.admit().is_err(), "{} {mode:o}", bytes.len());
            fixture.nothing_written();
        }
        // A symlink, even to a valid member, and a directory.
        let fixture = Fixture::new();
        let elsewhere = fixture.dir.join("elsewhere");
        fs::write(&elsewhere, record()).unwrap();
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o400)).unwrap();
        symlink(&elsewhere, fixture.member()).unwrap();
        assert!(fixture.admit().is_err());
        fixture.nothing_written();
        let fixture = Fixture::new();
        fs::create_dir(fixture.member()).unwrap();
        assert!(fixture.admit().is_err());
        assert!(!fixture.run().join("td-admit").exists());
    }

    /// The member's owner is checked as the key's is.
    #[test]
    fn a_member_of_another_owner_is_refused() {
        let fixture = Fixture::new();
        fixture.put(&record(), 0o400);
        let other = Owner {
            uid: fixture.owner.uid ^ 1,
            ..fixture.owner
        };
        let error = admit(&fixture.root(), &fixture.run(), other).unwrap_err();
        assert!(error.to_string().contains("owned by uid"), "{error}");
        fixture.nothing_written();
    }

    /// The directory is created fresh: one already there, of any kind,
    /// refuses and is left as it was, with the member still removed.
    #[test]
    fn an_existing_directory_is_never_reused() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.run().join("td-admit")).unwrap();
        fixture.put(&record(), 0o400);
        assert!(fixture.admit().is_err());
        assert!(fixture.written().is_none());
        assert!(fs::symlink_metadata(fixture.member()).is_err());

        let fixture = Fixture::new();
        let target = fixture.dir.join("target");
        fs::create_dir(&target).unwrap();
        symlink(&target, fixture.run().join("td-admit")).unwrap();
        fixture.put(&record(), 0o400);
        assert!(fixture.admit().is_err());
        assert!(fs::read_dir(&target).unwrap().next().is_none());
    }

    /// A failed write leaves nothing behind it.
    #[test]
    fn a_failed_write_writes_nothing() {
        let fixture = Fixture::new();
        fixture.put(&record(), 0o400);
        fs::set_permissions(fixture.run(), fs::Permissions::from_mode(0o500)).unwrap();
        let result = fixture.admit();
        fs::set_permissions(fixture.run(), fs::Permissions::from_mode(0o755)).unwrap();
        // Root may write a mode-0500 directory; then the write succeeds.
        if result.is_err() {
            fixture.nothing_written();
        }
        // A directory of another group is refused after its creation, and
        // removed.
        let fixture = Fixture::new();
        fixture.put(&record(), 0o400);
        let other = Owner {
            gid: fixture.owner.gid ^ 1,
            ..fixture.owner
        };
        // The member's owner check reads only the UID, as the key's does.
        assert!(admit(&fixture.root(), &fixture.run(), other).is_err());
        fixture.nothing_written();
    }
}
