//! Private ciphertext publication. The backend supplies an admitted directory and authorization.

use super::{LockedVault, MAX_ENVELOPE};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;

type Result<T> = std::result::Result<T, String>;
const FLAGS: i32 = 0o400000 | 0o4000; // O_NOFOLLOW | O_NONBLOCK
const VAULT: &str = "vault";
const LOCK: &str = "lock";

fn path(directory: &File, name: &str) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name)
}

fn admitted(file: &File, owner: u32, directory: bool) -> Result<Metadata> {
    let meta = file
        .metadata()
        .map_err(|_| "inspect portable store object")?;
    let valid = if directory {
        meta.is_dir() && meta.mode() & 0o7777 == 0o700 && meta.nlink() != 0
    } else {
        meta.is_file() && meta.mode() & 0o7777 == 0o600 && meta.nlink() == 1
    };
    if meta.uid() != owner || !valid {
        return Err("portable store owner, mode, type, or links refused".into());
    }
    Ok(meta)
}

fn same_inode(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

fn named(directory: &File, name: &str, file: &File) -> Result<()> {
    let current = fs::symlink_metadata(path(directory, name))
        .map_err(|_| "portable store object disappeared")?;
    let retained = file
        .metadata()
        .map_err(|_| "inspect retained portable object")?;
    if !same_inode(&current, &retained) || current.file_type().is_symlink() {
        return Err("portable store object replaced".into());
    }
    Ok(())
}

fn admit_lock(directory: &File, lock: &File, owner: u32, created: bool) -> Result<()> {
    let result = admitted(lock, owner, false).and_then(|metadata| {
        if metadata.len() == 0 {
            Ok(())
        } else {
            Err("portable lock is not empty".into())
        }
    });
    if result.is_err() && created && named(directory, LOCK, lock).is_ok() {
        fs::remove_file(path(directory, LOCK))
            .map_err(|_| "portable lock admission failed; temporary lock cleanup failed")?;
    }
    result
}

struct Record {
    file: File,
    vault: LockedVault,
}

/// Pins both the directory and the exact baseline inode across authentication.
/// Parsed ciphertext remains untrusted until the backend authenticates it.
pub(super) struct Snapshot {
    directory: File,
    record: Option<Record>,
}
impl Snapshot {
    pub fn vault(&self) -> Option<&LockedVault> {
        self.record.as_ref().map(|record| &record.vault)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PublishError {
    Rejected(String),
    /// Rename was attempted. Reload and authenticate; never retry automatically.
    Uncertain(String),
}
impl From<String> for PublishError {
    fn from(error: String) -> Self {
        Self::Rejected(error)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Created,
    Written,
    FileSynced,
    RenameAttempted,
    Renamed,
    DirectorySynced,
}

/// A short exclusive transaction; drop before prompting for a token.
/// No pathname acquisition, key handling, authentication or plaintext I/O.
pub(super) struct Store {
    directory: File,
    lock: File,
    owner: u32,
}
impl Store {
    /// The caller must acquire this directory through its trusted path adapter.
    /// Ownership is supplied by that adapter, never inferred as authority.
    pub fn open(directory: File, owner: u32) -> Result<Self> {
        admitted(&directory, owner, true)?;
        let lock_path = path(&directory, LOCK);
        let (lock, created) = match OpenOptions::new()
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(FLAGS)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => (file, true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(FLAGS)
                    .open(lock_path)
                    .map_err(|_| "open portable lock")?,
                false,
            ),
            Err(_) => return Err("create portable lock".into()),
        };
        admit_lock(&directory, &lock, owner, created)?;
        lock.try_lock()
            .map_err(|_| "portable store is busy or locking unavailable")?;
        let store = Self {
            directory,
            lock,
            owner,
        };
        store.check()?;
        if created {
            store.lock.sync_all().map_err(|_| "sync portable lock")?;
            store
                .directory
                .sync_all()
                .map_err(|_| "sync portable directory")?;
        }
        Ok(store)
    }

    fn check(&self) -> Result<()> {
        admitted(&self.directory, self.owner, true)?;
        if admitted(&self.lock, self.owner, false)?.len() != 0 {
            return Err("portable lock is not empty".into());
        }
        named(&self.directory, LOCK, &self.lock)
    }

    fn read(&self) -> Result<Option<Record>> {
        self.check()?;
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(FLAGS)
            .open(path(&self.directory, VAULT))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("open portable vault".into()),
        };
        let before = admitted(&file, self.owner, false)?;
        if before.len() > MAX_ENVELOPE as u64 {
            return Err("portable vault exceeds size limit".into());
        }
        let mut bytes = Vec::with_capacity(before.len() as usize);
        (&mut file)
            .take(MAX_ENVELOPE as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "read portable vault")?;
        let after = admitted(&file, self.owner, false)?;
        if bytes.len() as u64 != before.len()
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return Err("portable vault changed during read".into());
        }
        named(&self.directory, VAULT, &file)?;
        let vault = LockedVault::decode(&bytes)?;
        Ok(Some(Record { file, vault }))
    }

    pub fn load(&self) -> Result<Snapshot> {
        let record = self.read()?;
        let directory = self
            .directory
            .try_clone()
            .map_err(|_| "retain portable directory")?;
        Ok(Snapshot { directory, record })
    }

    fn baseline(&self, snapshot: &Snapshot) -> Result<()> {
        let retained = snapshot
            .directory
            .metadata()
            .map_err(|_| "inspect snapshot directory")?;
        if !same_inode(&retained, &admitted(&self.directory, self.owner, true)?) {
            return Err("portable snapshot belongs to another directory".into());
        }
        match (self.read()?, &snapshot.record) {
            (None, None) => Ok(()),
            (Some(current), Some(prior)) => {
                let prior_meta = prior.file.metadata().map_err(|_| "inspect snapshot file")?;
                let current_meta = current
                    .file
                    .metadata()
                    .map_err(|_| "inspect current file")?;
                if same_inode(&prior_meta, &current_meta)
                    && current.vault.bytes() == prior.vault.bytes()
                {
                    Ok(())
                } else {
                    Err("portable save baseline changed".into())
                }
            }
            _ => Err("portable save baseline changed".into()),
        }
    }

    /// Structural sequencing only; the backend must authenticate the baseline,
    /// prove initial protectors, and authorize the exact proposed operation.
    pub fn publish(
        self,
        snapshot: Snapshot,
        proposed: &LockedVault,
        random: &mut impl Read,
    ) -> std::result::Result<(), PublishError> {
        self.publish_inner(snapshot, proposed, random, &mut |_| Ok(()))
    }

    fn publish_inner(
        self,
        snapshot: Snapshot,
        proposed: &LockedVault,
        random: &mut impl Read,
        step: &mut impl FnMut(Stage) -> Result<()>,
    ) -> std::result::Result<(), PublishError> {
        self.baseline(&snapshot)?;
        match snapshot.vault() {
            None if proposed.revision == 1 => {}
            Some(prior)
                if proposed.id == prior.id
                    && prior.revision.checked_add(1) == Some(proposed.revision) => {}
            _ => {
                return Err(PublishError::Rejected(
                    "portable publication sequence refused".into(),
                ))
            }
        }
        let mut suffix = [0; 16];
        random
            .read_exact(&mut suffix)
            .map_err(|_| PublishError::Rejected("portable temporary entropy unavailable".into()))?;
        let name = format!(
            "tmp-{}",
            suffix
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let temporary = path(&self.directory, &name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(FLAGS)
            .open(&temporary)
            .map_err(|_| PublishError::Rejected("create portable temporary".into()))?;
        let mut rename_attempted = false;
        let result = (|| -> Result<()> {
            admitted(&file, self.owner, false)?;
            step(Stage::Created)?;
            file.write_all(proposed.bytes())
                .map_err(|_| "write portable ciphertext")?;
            step(Stage::Written)?;
            file.sync_all().map_err(|_| "sync portable ciphertext")?;
            step(Stage::FileSynced)?;
            self.baseline(&snapshot)?;
            admitted(&file, self.owner, false)?;
            named(&self.directory, &name, &file)?;
            rename_attempted = true;
            step(Stage::RenameAttempted)?;
            fs::rename(&temporary, path(&self.directory, VAULT))
                .map_err(|_| "publish portable ciphertext")?;
            step(Stage::Renamed)?;
            self.directory
                .sync_all()
                .map_err(|_| "sync portable publication")?;
            step(Stage::DirectorySynced)?;
            self.check()?;
            named(&self.directory, VAULT, &file)?;
            Ok(())
        })();
        match result {
            Ok(()) => Ok(()),
            Err(error) if rename_attempted => Err(PublishError::Uncertain(error)),
            Err(error) => {
                // Never remove an object that displaced our temporary.
                if named(&self.directory, &name, &file).is_ok() {
                    let _ = fs::remove_file(&temporary);
                }
                Err(PublishError::Rejected(error))
            }
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // An inherited open-file description must not extend this transaction.
        let _ = self.lock.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::{tests::fixture, Entry, Notebook, Secret32};
    use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "td-portable-store-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn owner(&self) -> u32 {
            fs::metadata(&self.0).unwrap().uid()
        }
        fn open(&self) -> Store {
            Store::open(File::open(&self.0).unwrap(), self.owner()).unwrap()
        }
        fn read(&self) -> Snapshot {
            self.open().load().unwrap()
        }
        fn seed(&self) {
            let snapshot = self.read();
            self.open()
                .publish(snapshot, &fixture(), &mut io::repeat(0x55))
                .unwrap();
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn write_private(path: &std::path::Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
    }
    fn revision(snapshot: &Snapshot, value: u8) -> LockedVault {
        let vault = snapshot.vault().unwrap();
        let opened = vault.open(b"primary", &Secret32([0x11; 32])).unwrap();
        let notebook = Notebook {
            entries: vec![Entry::new(
                [0x33; 16],
                2,
                "Changed entry".into(),
                vec![value],
            )],
        };
        vault
            .revise(&opened, &notebook, &mut io::repeat(value))
            .unwrap()
    }
    fn assert_open(snapshot: &Snapshot, body: &[u8]) {
        for (id, key) in [(b"primary".as_slice(), 0x11), (b"backup".as_slice(), 0x22)] {
            let opened = snapshot
                .vault()
                .unwrap()
                .open(id, &Secret32([key; 32]))
                .unwrap();
            assert_eq!(opened.notebook.entries[0].body(), body);
        }
    }

    #[test]
    fn encrypted_creation_and_revision_reopen_with_each_key() {
        let directory = Directory::new();
        let absent = directory.read();
        assert!(absent.vault().is_none());
        let vault = fixture();
        directory
            .open()
            .publish(absent, &vault, &mut io::repeat(1))
            .unwrap();
        let snapshot = directory.read();
        assert_eq!(fs::read(directory.0.join(VAULT)).unwrap(), vault.bytes());
        assert_open(&snapshot, b"username: alice\npassword: example\r\n");
        let next = revision(&snapshot, b'z');
        directory
            .open()
            .publish(snapshot, &next, &mut io::repeat(2))
            .unwrap();
        assert_open(&directory.read(), b"z");
        for entry in fs::read_dir(&directory.0).unwrap() {
            let entry = entry.unwrap();
            assert!([VAULT, LOCK].contains(&entry.file_name().to_str().unwrap()));
            assert_eq!(entry.metadata().unwrap().mode() & 0o7777, 0o600);
        }
    }

    #[test]
    fn competing_snapshots_and_first_creators_never_overwrite() {
        let directory = Directory::new();
        let first = directory.read();
        let late = directory.read();
        directory
            .open()
            .publish(first, &fixture(), &mut io::repeat(1))
            .unwrap();
        assert!(matches!(
            directory
                .open()
                .publish(late, &fixture(), &mut io::repeat(2)),
            Err(PublishError::Rejected(_))
        ));
        let first = directory.read();
        let late = directory.read();
        let a = revision(&first, b'a');
        let b = revision(&late, b'b');
        directory
            .open()
            .publish(first, &a, &mut io::repeat(3))
            .unwrap();
        assert!(matches!(
            directory.open().publish(late, &b, &mut io::repeat(4)),
            Err(PublishError::Rejected(_))
        ));
        assert_open(&directory.read(), b"a");
    }

    #[test]
    fn publication_requires_same_directory_inode_bytes_identity_and_next_revision() {
        let a = Directory::new();
        let b = Directory::new();
        a.seed();
        b.seed();
        let snapshot = a.read();
        let next = revision(&snapshot, b'n');
        assert!(b
            .open()
            .publish(snapshot, &next, &mut io::repeat(1))
            .is_err());
        let snapshot = a.read();
        let bytes = snapshot.vault().unwrap().bytes().to_vec();
        // Identical bytes at a new inode still invalidate the saved baseline.
        write_private(&a.0.join("replacement"), &bytes);
        fs::rename(a.0.join("replacement"), a.0.join(VAULT)).unwrap();
        assert!(a
            .open()
            .publish(snapshot, &next, &mut io::repeat(1))
            .is_err());
        let snapshot = a.read();
        let mut changed = bytes.clone();
        *changed.last_mut().unwrap() ^= 1;
        fs::write(a.0.join(VAULT), &changed).unwrap();
        assert!(a
            .open()
            .publish(snapshot, &next, &mut io::repeat(1))
            .is_err());
        fs::write(a.0.join(VAULT), &bytes).unwrap();
        let snapshot = a.read();
        assert!(a
            .open()
            .publish(snapshot, &fixture(), &mut io::repeat(1))
            .is_err());
        for offset in [8, 47] {
            let mut wrong = next.bytes().to_vec();
            wrong[offset] ^= 1;
            let wrong = LockedVault::decode(&wrong).unwrap();
            let snapshot = a.read();
            assert!(a
                .open()
                .publish(snapshot, &wrong, &mut io::repeat(1))
                .is_err());
        }
        let empty = Directory::new();
        let snapshot = empty.read();
        assert!(empty
            .open()
            .publish(snapshot, &next, &mut io::repeat(1))
            .is_err());
        assert_eq!(fs::read(a.0.join(VAULT)).unwrap(), bytes);
    }

    #[test]
    fn directory_rename_cannot_redirect_the_pinned_store() {
        let directory = Directory::new();
        directory.seed();
        let snapshot = directory.read();
        let next = revision(&snapshot, b'p');
        let store = directory.open();
        let moved = directory.0.with_extension("moved");
        fs::rename(&directory.0, &moved).unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory.0)
            .unwrap();
        store.publish(snapshot, &next, &mut io::repeat(3)).unwrap();
        assert!(!directory.0.join(VAULT).exists());
        let owner = fs::metadata(&moved).unwrap().uid();
        let actual = Store::open(File::open(&moved).unwrap(), owner)
            .unwrap()
            .load()
            .unwrap();
        assert_open(&actual, b"p");
        fs::remove_dir_all(moved).unwrap();
    }

    #[test]
    fn rejected_new_lock_is_cleaned_but_existing_or_replaced_locks_are_preserved() {
        let directory = Directory::new();
        let parent = File::open(&directory.0).unwrap();
        for existing in [true, false] {
            write_private(&directory.0.join(LOCK), &[]);
            let lock = File::open(directory.0.join(LOCK)).unwrap();
            lock.set_permissions(fs::Permissions::from_mode(0o0)).unwrap();
            assert!(admit_lock(&parent, &lock, directory.owner(), !existing).is_err());
            assert_eq!(directory.0.join(LOCK).exists(), existing);
            if existing {
                fs::remove_file(directory.0.join(LOCK)).unwrap();
            }
        }
        write_private(&directory.0.join(LOCK), &[]);
        let old = File::open(directory.0.join(LOCK)).unwrap();
        fs::rename(directory.0.join(LOCK), directory.0.join("old-lock")).unwrap();
        write_private(&directory.0.join(LOCK), b"replacement");
        assert!(admit_lock(&parent, &old, directory.owner() ^ 1, true).is_err());
        assert_eq!(fs::read(directory.0.join(LOCK)).unwrap(), b"replacement");
    }

    #[test]
    fn retirement_unlocks_even_with_an_inherited_open_file_description() {
        let directory = Directory::new();
        let store = directory.open();
        let inherited = store.lock.try_clone().unwrap();
        drop(store);
        let next = directory.open();
        // Closing the old description must not unlock a newer transaction.
        drop(inherited);
        assert!(Store::open(File::open(&directory.0).unwrap(), directory.owner()).is_err());
        drop(next);
        assert!(directory.read().vault().is_none());
    }

    #[test]
    fn locks_contend_and_replaced_or_corrupt_locks_are_refused() {
        let directory = Directory::new();
        let store = directory.open();
        assert!(Store::open(File::open(&directory.0).unwrap(), directory.owner()).is_err());
        fs::rename(directory.0.join(LOCK), directory.0.join("old-lock")).unwrap();
        write_private(&directory.0.join(LOCK), &[]);
        assert!(store.load().is_err());
        drop(store);
        let store = directory.open();
        fs::write(directory.0.join(LOCK), b"unexpected").unwrap();
        assert!(store.load().is_err());
        drop(store);
        assert!(Store::open(File::open(&directory.0).unwrap(), directory.owner()).is_err());
    }

    #[test]
    fn foreign_owners_permissions_links_symlinks_types_and_oversize_are_refused() {
        let directory = Directory::new();
        assert!(Store::open(File::open(&directory.0).unwrap(), directory.owner() ^ 1).is_err());
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(Store::open(File::open(&directory.0).unwrap(), directory.owner()).is_err());
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700)).unwrap();
        for name in [LOCK, VAULT] {
            let object = directory.0.join(name);
            symlink("missing", &object).unwrap();
            let load = || {
                Store::open(File::open(&directory.0).unwrap(), directory.owner())
                    .and_then(|store| store.load())
            };
            assert!(load().is_err());
            fs::remove_file(&object).unwrap();
            fs::create_dir(&object).unwrap();
            assert!(load().is_err());
            fs::remove_dir(&object).unwrap();
            let vault = fixture();
            write_private(&object, if name == LOCK { &[] } else { vault.bytes() });
            fs::hard_link(&object, directory.0.join("alias")).unwrap();
            assert!(load().is_err());
            fs::remove_file(directory.0.join("alias")).unwrap();
            fs::set_permissions(&object, fs::Permissions::from_mode(0o640)).unwrap();
            assert!(load().is_err());
            fs::remove_file(object).unwrap();
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.0.join(VAULT))
            .unwrap();
        file.set_len(MAX_ENVELOPE as u64 + 1).unwrap();
        assert!(directory.open().load().is_err());
        file.set_len(0).unwrap();
        assert!(directory.open().load().is_err());
    }

    #[test]
    fn failures_before_rename_preserve_old_bytes_after_rename_require_reload() {
        for stage in [
            Stage::Created,
            Stage::Written,
            Stage::FileSynced,
            Stage::RenameAttempted,
            Stage::Renamed,
            Stage::DirectorySynced,
        ] {
            for initial in [true, false] {
                let directory = Directory::new();
                if !initial {
                    directory.seed();
                }
                let snapshot = directory.read();
                let proposed = if initial {
                    fixture()
                } else {
                    revision(&snapshot, b'f')
                };
                let before = snapshot.vault().map(|v| v.bytes().to_vec());
                let result = directory.open().publish_inner(
                    snapshot,
                    &proposed,
                    &mut io::repeat(7),
                    &mut |point| {
                        if point == stage {
                            Err("injected".into())
                        } else {
                            Ok(())
                        }
                    },
                );
                let after = directory.read();
                if matches!(stage, Stage::Renamed | Stage::DirectorySynced) {
                    assert_eq!(result, Err(PublishError::Uncertain("injected".into())));
                    assert_eq!(after.vault().unwrap().bytes(), proposed.bytes());
                } else {
                    if stage == Stage::RenameAttempted {
                        assert_eq!(result, Err(PublishError::Uncertain("injected".into())));
                    } else {
                        assert!(matches!(result, Err(PublishError::Rejected(_))));
                    }
                    assert_eq!(after.vault().map(|v| v.bytes()), before.as_deref());
                }
                if stage == Stage::RenameAttempted {
                    assert!(directory
                        .0
                        .join(format!("tmp-{}", "07".repeat(16)))
                        .exists());
                    continue;
                }
                assert!(fs::read_dir(&directory.0).unwrap().all(|e| !e
                    .unwrap()
                    .file_name()
                    .to_str()
                    .unwrap()
                    .starts_with("tmp-")));
            }
        }
    }

    #[test]
    fn entropy_failure_and_orphan_collision_leave_vault_and_orphan_untouched() {
        let directory = Directory::new();
        directory.seed();
        let snapshot = directory.read();
        let next = revision(&snapshot, b'o');
        assert!(directory
            .open()
            .publish(snapshot, &next, &mut io::empty())
            .is_err());
        let orphan = directory.0.join(format!("tmp-{}", "07".repeat(16)));
        write_private(&orphan, b"orphan ciphertext");
        let snapshot = directory.read();
        assert!(directory
            .open()
            .publish(snapshot, &next, &mut io::repeat(7))
            .is_err());
        assert_eq!(fs::read(orphan).unwrap(), b"orphan ciphertext");
        assert_open(&directory.read(), b"username: alice\npassword: example\r\n");
    }

    #[test]
    fn final_check_refuses_interference_after_the_temporary_is_synced() {
        for target in ["vault", "lock", "temporary"] {
            let directory = Directory::new();
            directory.seed();
            let snapshot = directory.read();
            let proposed = revision(&snapshot, b'q');
            let original = snapshot.vault().unwrap().bytes().to_vec();
            let mut changed = original.clone();
            *changed.last_mut().unwrap() ^= 1;
            let result = directory.open().publish_inner(
                snapshot,
                &proposed,
                &mut io::repeat(8),
                &mut |stage| {
                    if stage == Stage::FileSynced {
                        match target {
                            "vault" => fs::write(directory.0.join(VAULT), &changed).unwrap(),
                            "lock" => {
                                fs::rename(directory.0.join(LOCK), directory.0.join("old-lock"))
                                    .unwrap();
                                write_private(&directory.0.join(LOCK), &[]);
                            }
                            _ => {
                                let path = directory.0.join(format!("tmp-{}", "08".repeat(16)));
                                fs::remove_file(&path).unwrap();
                                write_private(&path, b"displaced temporary");
                            }
                        }
                    }
                    Ok(())
                },
            );
            assert!(matches!(result, Err(PublishError::Rejected(_))));
            assert_eq!(
                fs::read(directory.0.join(VAULT)).unwrap(),
                if target == "vault" { changed } else { original }
            );
            if target == "temporary" {
                let path = directory.0.join(format!("tmp-{}", "08".repeat(16)));
                assert_eq!(fs::read(path).unwrap(), b"displaced temporary");
            }
        }
    }

    #[test]
    #[ignore = "owned subprocess crash fixture"]
    fn abrupt_writer() {
        let Ok(directory) = std::env::var("TD_PORTABLE_CRASH_DIRECTORY") else {
            return;
        };
        let stage = std::env::var("TD_PORTABLE_CRASH_STAGE").unwrap();
        let directory = File::open(directory).unwrap();
        let owner = directory.metadata().unwrap().uid();
        let store = Store::open(directory, owner).unwrap();
        let snapshot = store.load().unwrap();
        let proposed = revision(&snapshot, b'c');
        let _ = store.publish_inner(snapshot, &proposed, &mut io::repeat(9), &mut |point| {
            if (stage == "synced" && point == Stage::FileSynced)
                || (stage == "renamed" && point == Stage::Renamed)
            {
                std::process::exit(71);
            }
            Ok(())
        });
        panic!("crash checkpoint was not reached");
    }

    #[test]
    fn process_exit_releases_lock_and_never_exposes_a_partial_committed_vault() {
        for stage in ["synced", "renamed"] {
            let directory = Directory::new();
            directory.seed();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "portable::storage::tests::abrupt_writer",
                ])
                .env("TD_PORTABLE_CRASH_DIRECTORY", &directory.0)
                .env("TD_PORTABLE_CRASH_STAGE", stage)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(71));
            let snapshot = directory.read();
            assert_open(
                &snapshot,
                if stage == "synced" {
                    b"username: alice\npassword: example\r\n"
                } else {
                    b"c"
                },
            );
            let next = revision(&snapshot, b'r');
            directory
                .open()
                .publish(snapshot, &next, &mut io::repeat(10))
                .unwrap();
            assert_open(&directory.read(), b"r");
            // A pre-rename crash can leave ciphertext; it is never adopted.
            let orphan = directory.0.join(format!("tmp-{}", "09".repeat(16)));
            assert_eq!(orphan.exists(), stage == "synced");
        }
    }
}
