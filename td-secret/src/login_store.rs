//! Login record store (td-login/TOKEN-LOGIN.md, "The login record"): reads
//! the login state and publishes or removes `<directory>/<uid>`. Writers
//! are serialized by the caller; this module takes no lock.

use super::login_record::{Record, MAX_RECORD};
use super::login_state::{same_inode, Directory, Facts, NOFOLLOW};
pub(super) use super::login_state::{Cause, Owner, DIRECTORY, TEMPORARY};
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const NONBLOCK: i32 = 0o4000;

pub(super) enum State {
    Unenrolled,
    /// The record keeps the SHA-256 of the exact bytes read.
    Enrolled(Record),
    Unavailable(Cause),
}

impl State {
    /// What a write compares against; an unavailable state has none.
    pub fn baseline(&self) -> Option<Baseline> {
        match self {
            Self::Unenrolled => Some(Baseline::Absent),
            Self::Enrolled(record) => Some(Baseline::Digest(record.digest())),
            Self::Unavailable(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Baseline {
    Absent,
    Digest([u8; 32]),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// Published or removed, and the directory synced.
    Committed,
    /// Nothing changed at the record name.
    Rejected(String),
    /// The rename or unlink was attempted: re-read and report; never retry.
    Uncertain(String),
}

/// Read checkpoints for tests: after the name is inspected, after the bytes are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadStage {
    Inspected,
    Read,
}

/// Write checkpoints, each after its step, where tests inject a failure or
/// a concurrent change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Created,
    Written,
    FileSynced,
    RenameAttempted,
    Renamed,
    UnlinkAttempted,
    Unlinked,
    DirectorySynced,
}

fn unchanged_during_read(before: &Metadata, after: &Metadata) -> bool {
    same_inode(before, after)
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

/// The login state at `path`, read once.
pub(super) fn read(path: &Path, owner: Owner, uid: u32) -> State {
    match Store::open(path, owner, uid) {
        Ok(store) => store.read(),
        Err(cause) => State::Unavailable(cause),
    }
}

/// The login directory and the account whose record it holds.
pub(super) struct Store {
    directory: Directory,
    uid: u32,
}

impl Store {
    /// Walks `path` from `/` one component at a time without following a
    /// link (`login_state::walk`). A missing name counts only while the
    /// directory's own `/proc/self/fd` path names it; otherwise it is
    /// unreadable.
    pub fn open(path: &Path, owner: Owner, uid: u32) -> Result<Self, Cause> {
        Directory::open(path, owner)
            .map(|directory| Self { directory, uid })
            .map_err(|refusal| refusal.cause)
    }

    #[cfg(test)]
    fn open_via(fd_root: &Path, path: &Path, owner: Owner, uid: u32) -> Result<Self, Cause> {
        Directory::open_via(fd_root, path, owner)
            .map(|directory| Self { directory, uid })
            .map_err(|refusal| refusal.cause)
    }

    fn check_directory(&self) -> Result<(), Cause> {
        self.directory.check().map_err(|refusal| refusal.cause)
    }

    fn at(&self, name: impl AsRef<Path>) -> PathBuf {
        self.directory.at(name)
    }

    fn record_path(&self) -> PathBuf {
        self.at(self.uid.to_string())
    }

    /// Looks up only the exact record name; every other entry is ignored.
    pub fn read(&self) -> State {
        match self.read_record() {
            Ok(None) => State::Unenrolled,
            Ok(Some(record)) => State::Enrolled(record),
            Err(cause) => State::Unavailable(cause),
        }
    }

    fn read_record(&self) -> Result<Option<Record>, Cause> {
        self.read_record_inner(&mut |_| ())
    }

    fn read_record_inner(&self, step: &mut impl FnMut(ReadStage)) -> Result<Option<Record>, Cause> {
        // Inspect the name before opening it, so no device or FIFO is opened.
        let Some(named) = self
            .directory
            .lookup(&self.uid.to_string())
            .map_err(|refusal| refusal.cause)?
        else {
            return Ok(None);
        };
        let owner = self.directory.owner();
        if !Facts::of(&named).valid_record(owner) || named.len() > MAX_RECORD as u64 {
            return Err(Cause::RecordDamaged);
        }
        step(ReadStage::Inspected);
        // The name was a valid record a moment ago: a failure now is a race.
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(self.record_path())
            .map_err(|_| Cause::Unreadable)?;
        let before = file.metadata().map_err(|_| Cause::Unreadable)?;
        if !same_inode(&named, &before)
            || !Facts::of(&before).valid_record(owner)
            || before.len() > MAX_RECORD as u64
        {
            return Err(Cause::Unreadable);
        }
        let mut bytes = Vec::with_capacity(MAX_RECORD);
        (&mut file)
            .take(MAX_RECORD as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Cause::Unreadable)?;
        step(ReadStage::Read);
        let after = file.metadata().map_err(|_| Cause::Unreadable)?;
        if !unchanged_during_read(&before, &after) || bytes.len() as u64 != before.len() {
            return Err(Cause::Unreadable);
        }
        Record::decode(&bytes, self.uid)
            .map(Some)
            .map_err(|_| Cause::RecordDamaged)
    }

    fn current(&self) -> Result<Baseline, String> {
        match self.read_record() {
            Ok(None) => Ok(Baseline::Absent),
            Ok(Some(record)) => Ok(Baseline::Digest(record.digest())),
            Err(cause) => Err(format!("login state unavailable: {cause:?}")),
        }
    }

    fn unchanged(&self, baseline: Baseline) -> Result<(), String> {
        if self.current()? == baseline {
            Ok(())
        } else {
            Err("login record baseline changed".into())
        }
    }

    fn admit(&self, file: &File) -> Result<(), String> {
        let meta = file
            .metadata()
            .map_err(|_| "inspect login record temporary")?;
        if Facts::of(&meta).valid_record(self.directory.owner()) {
            Ok(())
        } else {
            Err("login record temporary owner, mode, type or links refused".into())
        }
    }

    /// Unlinks every name starting with `TEMPORARY`, and only those.
    pub fn remove_temporaries(&self) -> Result<(), String> {
        self.directory.remove_temporaries()
    }

    /// Publishes `record`, written in the version it was built with: the
    /// caller chose it with `login_record::write_version`.
    pub fn publish(&self, baseline: Baseline, record: &Record, random: &mut impl Read) -> Outcome {
        self.publish_inner(baseline, record, random, &mut |_| Ok(()))
    }

    fn publish_inner(
        &self,
        baseline: Baseline,
        record: &Record,
        random: &mut impl Read,
        step: &mut impl FnMut(Stage) -> Result<(), String>,
    ) -> Outcome {
        if record.uid() != self.uid {
            return Outcome::Rejected("login record is for another UID".into());
        }
        let bytes = match record.encode() {
            Ok(bytes) => bytes,
            Err(error) => return Outcome::Rejected(error),
        };
        if let Err(error) = self
            .remove_temporaries()
            .and_then(|()| self.unchanged(baseline))
        {
            return Outcome::Rejected(error);
        }
        let mut suffix = [0; 16];
        if random.read_exact(&mut suffix).is_err() {
            return Outcome::Rejected("login record temporary entropy unavailable".into());
        }
        let mut name = String::with_capacity(TEMPORARY.len() + 2 * suffix.len());
        name.push_str(TEMPORARY);
        for nibble in suffix.iter().flat_map(|byte| [byte >> 4, byte & 0xf]) {
            name.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
        }
        let temporary = self.at(&name);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(_) => return Outcome::Rejected("create login record temporary".into()),
        };
        let mut rename_attempted = false;
        let result = (|| -> Result<(), String> {
            // The umask may have narrowed the creation mode.
            file.set_permissions(Permissions::from_mode(0o600))
                .map_err(|_| "set login record temporary mode")?;
            self.admit(&file)?;
            step(Stage::Created)?;
            file.write_all(&bytes)
                .map_err(|_| "write login record temporary")?;
            step(Stage::Written)?;
            file.sync_all().map_err(|_| "sync login record temporary")?;
            step(Stage::FileSynced)?;
            self.unchanged(baseline)?;
            self.admit(&file)?;
            still_named(&temporary, &file)?;
            rename_attempted = true;
            step(Stage::RenameAttempted)?;
            fs::rename(&temporary, self.record_path()).map_err(|_| "publish login record")?;
            step(Stage::Renamed)?;
            self.directory
                .file()
                .sync_all()
                .map_err(|_| "sync login record publication")?;
            step(Stage::DirectorySynced)?;
            self.check_directory()
                .map_err(|cause| format!("login directory unavailable: {cause:?}"))?;
            still_named(&self.record_path(), &file)
        })();
        match result {
            Ok(()) => Outcome::Committed,
            Err(error) if rename_attempted => Outcome::Uncertain(error),
            Err(error) => {
                // Never remove an object that displaced the temporary.
                if still_named(&temporary, &file).is_ok() {
                    let _ = fs::remove_file(&temporary);
                }
                Outcome::Rejected(error)
            }
        }
    }

    /// Removes the last key: unlinks the record named by `baseline`.
    pub fn remove(&self, baseline: Baseline) -> Outcome {
        self.remove_inner(baseline, &mut |_| Ok(()))
    }

    /// The worker's tests' publication, its hook given each stage's name so
    /// `Stage` and the hooked writes stay private to this module.
    #[cfg(test)]
    pub(super) fn publish_at(
        &self,
        baseline: Baseline,
        record: &Record,
        random: &mut impl Read,
        hook: &mut impl FnMut(&str) -> Result<(), String>,
    ) -> Outcome {
        self.publish_inner(baseline, record, random, &mut |stage| {
            hook(&format!("{stage:?}"))
        })
    }

    /// The worker's tests' removal, hooked as `publish_at` is.
    #[cfg(test)]
    pub(super) fn remove_at(
        &self,
        baseline: Baseline,
        hook: &mut impl FnMut(&str) -> Result<(), String>,
    ) -> Outcome {
        self.remove_inner(baseline, &mut |stage| hook(&format!("{stage:?}")))
    }

    fn remove_inner(
        &self,
        baseline: Baseline,
        step: &mut impl FnMut(Stage) -> Result<(), String>,
    ) -> Outcome {
        if baseline == Baseline::Absent {
            return Outcome::Rejected("no login record to remove".into());
        }
        if let Err(error) = self
            .remove_temporaries()
            .and_then(|()| self.unchanged(baseline))
        {
            return Outcome::Rejected(error);
        }
        let result = (|| -> Result<(), String> {
            step(Stage::UnlinkAttempted)?;
            fs::remove_file(self.record_path()).map_err(|_| "remove login record")?;
            step(Stage::Unlinked)?;
            self.directory
                .file()
                .sync_all()
                .map_err(|_| "sync login record removal")?;
            step(Stage::DirectorySynced)?;
            self.check_directory()
                .map_err(|cause| format!("login directory unavailable: {cause:?}"))?;
            match fs::symlink_metadata(self.record_path()) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => self
                    .directory
                    .resolves()
                    .map_err(|refusal| format!("login directory unavailable: {:?}", refusal.cause)),
                _ => Err("login record name reappeared".into()),
            }
        })();
        match result {
            Ok(()) => Outcome::Committed,
            Err(error) => Outcome::Uncertain(error),
        }
    }
}

fn still_named(path: &Path, file: &File) -> Result<(), String> {
    let current = fs::symlink_metadata(path).map_err(|_| "login record object disappeared")?;
    let retained = file
        .metadata()
        .map_err(|_| "inspect retained login record object")?;
    if current.file_type().is_symlink() || !same_inode(&current, &retained) {
        return Err("login record object replaced".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto;
    use crate::fido_p256::PublicKey;
    use crate::login_record::{NewKey, VERSION};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{symlink, DirBuilderExt};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    const VECTORS: &str = include_str!("../tests/login_record_vectors.txt");
    const UID: u32 = 1000;
    const PUBLISH: &[Stage] = &[
        Stage::Created,
        Stage::Written,
        Stage::FileSynced,
        Stage::RenameAttempted,
        Stage::Renamed,
        Stage::DirectorySynced,
    ];
    const REMOVE: &[Stage] = &[
        Stage::UnlinkAttempted,
        Stage::Unlinked,
        Stage::DirectorySynced,
    ];

    fn vector(name: &str) -> Vec<u8> {
        let hex = VECTORS
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
            .unwrap();
        hex.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn three() -> Record {
        Record::decode(&vector("record"), UID).unwrap()
    }

    fn two() -> Record {
        three()
            .without(VERSION, &[&vector("slot2_credential")])
            .unwrap()
    }

    fn for_uid(uid: u32) -> Record {
        let x = vector("slot0_x").try_into().unwrap();
        let y = vector("slot0_y").try_into().unwrap();
        let key = NewKey {
            credential: b"A".to_vec(),
            key: PublicKey::from_coordinates(&x, &y).unwrap(),
            salt: [0; 32],
            output: &[0; 32],
        };
        Record::enroll(uid, [7; 32], VERSION, vec![key]).unwrap()
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Unenrolled,
        Enrolled([u8; 32]),
        Unavailable(Cause),
    }

    fn seen(state: State) -> Seen {
        match state {
            State::Unenrolled => Seen::Unenrolled,
            State::Enrolled(record) => Seen::Enrolled(record.digest()),
            State::Unavailable(cause) => Seen::Unavailable(cause),
        }
    }

    fn enrolled(record: &Record) -> Seen {
        Seen::Enrolled(record.digest())
    }

    struct Fixture {
        root: PathBuf,
        dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "td-login-store-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            let dir = root.join("login");
            fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            Self { root, dir }
        }

        fn owner(&self) -> Owner {
            let meta = fs::metadata(&self.root).unwrap();
            Owner {
                uid: meta.uid(),
                gid: meta.gid(),
            }
        }

        fn store(&self) -> Store {
            Store::open(&self.dir, self.owner(), UID).unwrap()
        }

        fn state(&self) -> Seen {
            seen(read(&self.dir, self.owner(), UID))
        }

        fn baseline(&self) -> Baseline {
            self.store().read().baseline().unwrap()
        }

        fn record(&self) -> PathBuf {
            self.dir.join(UID.to_string())
        }

        fn write(&self, name: &str, bytes: &[u8]) {
            let path = self.dir.join(name);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            file.set_permissions(Permissions::from_mode(0o600)).unwrap();
            file.write_all(bytes).unwrap();
        }

        fn seed(&self, record: &Record) {
            self.write(&UID.to_string(), &record.encode().unwrap());
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }

        fn publish(&self, baseline: Baseline, record: &Record) -> Outcome {
            self.store()
                .publish(baseline, record, &mut io::repeat(0x5a))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.dir, Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn the_state_follows_the_directory_and_the_exact_name() {
        let fixture = Fixture::new();
        assert_eq!(fixture.state(), Seen::Unenrolled);
        assert_eq!(fixture.baseline(), Baseline::Absent);
        fixture.seed(&three());
        let digest = crypto::digest(&vector("record"));
        assert_eq!(digest.to_vec(), vector("digest"));
        assert_eq!(fixture.state(), Seen::Enrolled(digest));
        assert_eq!(fixture.baseline(), Baseline::Digest(digest));
        // Another account's name is not this account's record.
        assert_eq!(
            seen(read(&fixture.dir, fixture.owner(), UID + 1)),
            Seen::Unenrolled
        );
        assert!(State::Unavailable(Cause::Unreadable).baseline().is_none());
        assert_eq!(DIRECTORY, "/var/lib/td/login");
        assert_eq!(Owner::ROOT, Owner { uid: 0, gid: 0 });
    }

    #[test]
    fn a_damaged_directory_is_unavailable() {
        let fixture = Fixture::new();
        let owner = fixture.owner();
        let damaged = Seen::Unavailable(Cause::DirectoryDamaged);
        let state = |path: &Path, owner: Owner| seen(read(path, owner, UID));
        for mode in [0o750, 0o701, 0o755, 0o500, 0o1700, 0o2700] {
            fs::set_permissions(&fixture.dir, Permissions::from_mode(mode)).unwrap();
            assert_eq!(state(&fixture.dir, owner), damaged, "{mode:o}");
        }
        fs::set_permissions(&fixture.dir, Permissions::from_mode(0o700)).unwrap();
        for other in [
            Owner {
                uid: owner.uid ^ 1,
                ..owner
            },
            Owner {
                gid: owner.gid ^ 1,
                ..owner
            },
        ] {
            assert_eq!(state(&fixture.dir, other), damaged);
            assert!(Store::open(&fixture.dir, other, UID).is_err());
        }
        assert_eq!(state(&fixture.root.join("absent"), owner), damaged);
        assert_eq!(state(Path::new("relative/login"), owner), damaged);
        fs::write(fixture.root.join("file"), b"").unwrap();
        assert_eq!(state(&fixture.root.join("file"), owner), damaged);
        // A link to a valid directory, as the leaf or above it.
        symlink(&fixture.dir, fixture.root.join("link")).unwrap();
        assert_eq!(state(&fixture.root.join("link"), owner), damaged);
        symlink(&fixture.root, fixture.root.join("parent")).unwrap();
        assert_eq!(state(&fixture.root.join("parent/login"), owner), damaged);
        assert_eq!(state(&fixture.dir.join("../login"), owner), damaged);
        assert_eq!(state(&fixture.dir, owner), Seen::Unenrolled);
        // An opened store rechecks on every read and refuses to write.
        let store = fixture.store();
        fs::set_permissions(&fixture.dir, Permissions::from_mode(0o750)).unwrap();
        assert_eq!(seen(store.read()), damaged);
        assert!(store.remove_temporaries().is_err());
        assert!(matches!(
            store.publish(Baseline::Absent, &three(), &mut io::repeat(1)),
            Outcome::Rejected(_)
        ));
        fs::set_permissions(&fixture.dir, Permissions::from_mode(0o700)).unwrap();
        // A removed directory's descriptor never reads as an empty one.
        fs::remove_dir(&fixture.dir).unwrap();
        assert_eq!(seen(store.read()), damaged);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&fixture.dir)
            .unwrap();
    }

    #[test]
    fn a_damaged_record_is_unavailable() {
        let fixture = Fixture::new();
        let damaged = Seen::Unavailable(Cause::RecordDamaged);
        let bytes = three().encode().unwrap();
        let record = fixture.record();
        let repaired = |fixture: &Fixture| {
            let _ = fs::remove_file(fixture.record());
            fixture.seed(&three());
            assert_eq!(fixture.state(), enrolled(&three()));
            fs::remove_file(fixture.record()).unwrap();
        };
        // A link, even to a valid record.
        fixture.write("copy", &bytes);
        symlink("copy", &record).unwrap();
        assert_eq!(fixture.state(), damaged);
        fs::remove_file(&record).unwrap();
        symlink("missing", &record).unwrap();
        assert_eq!(fixture.state(), damaged);
        repaired(&fixture);
        // A second link.
        fixture.seed(&three());
        fs::hard_link(&record, fixture.dir.join("alias")).unwrap();
        assert_eq!(fixture.state(), damaged);
        fs::remove_file(fixture.dir.join("alias")).unwrap();
        assert_eq!(fixture.state(), enrolled(&three()));
        fs::remove_file(&record).unwrap();
        // Not a regular file.
        fs::create_dir(&record).unwrap();
        assert_eq!(fixture.state(), damaged);
        fs::remove_dir(&record).unwrap();
        let listener = UnixListener::bind(&record).unwrap();
        assert_eq!(fixture.state(), damaged);
        drop(listener);
        repaired(&fixture);
        // Any mode but 0600.
        for mode in [0o640, 0o400, 0o644, 0o700, 0o4600] {
            fixture.seed(&three());
            fs::set_permissions(&record, Permissions::from_mode(mode)).unwrap();
            assert_eq!(fixture.state(), damaged, "{mode:o}");
            fs::remove_file(&record).unwrap();
        }
        // Oversized, truncated, empty, trailing or another account's bytes.
        let mut oversized = bytes.clone();
        oversized.resize(MAX_RECORD + 1, 0);
        let mut trailing = bytes.clone();
        trailing.push(0);
        for contents in [
            oversized,
            vec![0; MAX_RECORD],
            bytes.get(..bytes.len() - 1).unwrap().to_vec(),
            Vec::new(),
            trailing,
            for_uid(UID + 1).encode().unwrap(),
        ] {
            fixture.write(&UID.to_string(), &contents);
            assert_eq!(fixture.state(), damaged, "{}", contents.len());
            fs::remove_file(&record).unwrap();
        }
        repaired(&fixture);
    }

    #[test]
    fn checks_pin_owner_group_type_mode_and_links() {
        let owner = Owner { uid: 0, gid: 0 };
        let facts = |directory, regular, mode, links, uid, gid| Facts {
            directory,
            regular,
            mode,
            links,
            uid,
            gid,
        };
        assert!(facts(true, false, 0o700, 2, 0, 0).valid_directory(owner));
        assert!(facts(false, true, 0o600, 1, 0, 0).valid_record(owner));
        for wrong in [
            facts(true, false, 0o700, 2, 1000, 0),
            facts(true, false, 0o700, 2, 0, 1000),
            facts(true, false, 0o750, 2, 0, 0),
            facts(true, false, 0o700, 0, 0, 0),
            facts(false, true, 0o700, 1, 0, 0),
        ] {
            assert!(!wrong.valid_directory(owner));
        }
        for wrong in [
            facts(false, true, 0o600, 1, 1000, 0),
            facts(false, true, 0o600, 1, 0, 1000),
            facts(false, true, 0o640, 1, 0, 0),
            facts(false, true, 0o600, 2, 0, 0),
            facts(false, true, 0o600, 0, 0, 0),
            facts(false, false, 0o600, 1, 0, 0),
            facts(true, false, 0o600, 1, 0, 0),
        ] {
            assert!(!wrong.valid_record(owner));
        }
    }

    /// The store and the shared predicate agree on every state the
    /// directory and the name decide.
    #[test]
    fn the_store_and_the_shared_predicate_agree() {
        use crate::login_state::{self, state_as};
        let fixture = Fixture::new();
        let owner = fixture.owner();
        let root = fixture.root.join("root");
        let login = login_state::directory(&root);
        fs::DirBuilder::new()
            .mode(0o755)
            .recursive(true)
            .create(login.parent().unwrap())
            .unwrap();
        fs::DirBuilder::new().mode(0o700).create(&login).unwrap();
        let agree = |shared: login_state::State| {
            let stored = match read(&login, owner, UID) {
                State::Unenrolled => login_state::State::Unenrolled,
                State::Enrolled(_) => login_state::State::Enrolled,
                State::Unavailable(cause) => login_state::State::Unavailable(cause),
            };
            assert_eq!(state_as(&root, owner, UID), stored);
            assert_eq!(stored, shared);
        };
        agree(login_state::State::Unenrolled);
        fs::write(login.join("tmp-1"), b"").unwrap();
        agree(login_state::State::Unenrolled);
        fs::write(login.join(UID.to_string()), three().encode().unwrap()).unwrap();
        fs::set_permissions(login.join(UID.to_string()), Permissions::from_mode(0o600)).unwrap();
        agree(login_state::State::Enrolled);
        fs::remove_file(login.join(UID.to_string())).unwrap();
        fs::set_permissions(&login, Permissions::from_mode(0o750)).unwrap();
        agree(login_state::State::Unavailable(Cause::DirectoryDamaged));
        fs::set_permissions(&login, Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(&login).unwrap();
        agree(login_state::State::Unavailable(Cause::DirectoryDamaged));
    }

    #[test]
    fn publication_and_removal_round_trip() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.publish(Baseline::Absent, &three()),
            Outcome::Committed
        );
        assert_eq!(fixture.state(), enrolled(&three()));
        assert_eq!(fs::read(fixture.record()).unwrap(), vector("record"));
        let meta = fs::symlink_metadata(fixture.record()).unwrap();
        assert!(Facts::of(&meta).valid_record(fixture.owner()));
        assert_eq!(fixture.names(), ["1000"]);
        let baseline = fixture.baseline();
        assert_eq!(fixture.publish(baseline, &two()), Outcome::Committed);
        assert_eq!(fixture.state(), enrolled(&two()));
        let baseline = fixture.baseline();
        assert_eq!(fixture.store().remove(baseline), Outcome::Committed);
        assert_eq!(fixture.state(), Seen::Unenrolled);
        assert!(fixture.names().is_empty());
    }

    #[test]
    fn writes_refuse_another_account_missing_entropy_and_an_absent_removal() {
        let fixture = Fixture::new();
        let other = Store::open(&fixture.dir, fixture.owner(), UID + 1).unwrap();
        assert_eq!(
            other.publish(Baseline::Absent, &three(), &mut io::repeat(1)),
            Outcome::Rejected("login record is for another UID".into())
        );
        assert!(matches!(
            fixture
                .store()
                .publish(Baseline::Absent, &three(), &mut io::empty()),
            Outcome::Rejected(_)
        ));
        assert_eq!(
            fixture.store().remove(Baseline::Absent),
            Outcome::Rejected("no login record to remove".into())
        );
        assert!(fixture.names().is_empty());
    }

    #[test]
    fn injected_publication_failures_leave_the_old_or_the_whole_new_record() {
        for &stage in PUBLISH {
            for initial in [true, false] {
                let fixture = Fixture::new();
                let old = if initial {
                    Seen::Unenrolled
                } else {
                    fixture.seed(&three());
                    enrolled(&three())
                };
                let proposed = two();
                let outcome = fixture.store().publish_inner(
                    fixture.baseline(),
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
                let temporary = format!("{TEMPORARY}{}", "07".repeat(16));
                let renamed = matches!(stage, Stage::Renamed | Stage::DirectorySynced);
                match stage {
                    Stage::Created | Stage::Written | Stage::FileSynced => {
                        assert_eq!(outcome, Outcome::Rejected("injected".into()));
                    }
                    _ => assert_eq!(outcome, Outcome::Uncertain("injected".into())),
                }
                let expected = if renamed { enrolled(&proposed) } else { old };
                assert_eq!(fixture.state(), expected, "{stage:?} {initial}");
                assert_eq!(
                    fixture.names().contains(&temporary),
                    stage == Stage::RenameAttempted,
                    "{stage:?}"
                );
                // A later write starts from what a re-read shows.
                let baseline = fixture.baseline();
                assert_eq!(fixture.publish(baseline, &three()), Outcome::Committed);
                assert_eq!(fixture.state(), enrolled(&three()));
                assert_eq!(fixture.names(), ["1000"]);
            }
        }
    }

    #[test]
    fn injected_removal_failures_are_uncertain_and_never_partial() {
        for &stage in REMOVE {
            let fixture = Fixture::new();
            fixture.seed(&three());
            let outcome = fixture
                .store()
                .remove_inner(fixture.baseline(), &mut |point| {
                    if point == stage {
                        Err("injected".into())
                    } else {
                        Ok(())
                    }
                });
            assert_eq!(outcome, Outcome::Uncertain("injected".into()));
            let expected = if stage == Stage::UnlinkAttempted {
                enrolled(&three())
            } else {
                Seen::Unenrolled
            };
            assert_eq!(fixture.state(), expected, "{stage:?}");
        }
    }

    #[test]
    fn a_changed_baseline_is_refused() {
        // A record added since the baseline was read.
        let fixture = Fixture::new();
        let absent = fixture.baseline();
        assert_eq!(fixture.publish(absent, &two()), Outcome::Committed);
        assert_eq!(
            fixture.publish(absent, &three()),
            Outcome::Rejected("login record baseline changed".into())
        );
        assert_eq!(fixture.state(), enrolled(&two()));
        // A record removed since the baseline was read.
        let present = fixture.baseline();
        assert_eq!(fixture.store().remove(present), Outcome::Committed);
        for outcome in [
            fixture.publish(present, &three()),
            fixture.store().remove(present),
        ] {
            assert_eq!(
                outcome,
                Outcome::Rejected("login record baseline changed".into())
            );
        }
        assert_eq!(fixture.state(), Seen::Unenrolled);
        // A record replaced since the baseline was read.
        fixture.seed(&three());
        let stale = fixture.baseline();
        fs::remove_file(fixture.record()).unwrap();
        fixture.seed(&two());
        assert!(matches!(
            fixture.store().remove(stale),
            Outcome::Rejected(_)
        ));
        assert_eq!(fixture.state(), enrolled(&two()));
        fs::remove_file(fixture.record()).unwrap();
        // Added or removed between the temporary's sync and the rename.
        for initial in [true, false] {
            if !initial {
                fixture.seed(&three());
            }
            let baseline = fixture.baseline();
            let outcome =
                fixture
                    .store()
                    .publish_inner(baseline, &two(), &mut io::repeat(3), &mut |stage| {
                        if stage == Stage::FileSynced {
                            if initial {
                                fixture.seed(&three());
                            } else {
                                fs::remove_file(fixture.record()).unwrap();
                            }
                        }
                        Ok(())
                    });
            assert_eq!(
                outcome,
                Outcome::Rejected("login record baseline changed".into())
            );
            let expected = if initial {
                enrolled(&three())
            } else {
                Seen::Unenrolled
            };
            assert_eq!(fixture.state(), expected);
            let _ = fs::remove_file(fixture.record());
            assert!(fixture.names().is_empty());
        }
        // A damaged record is no absent baseline.
        fixture.write(&UID.to_string(), b"damaged");
        assert!(matches!(
            fixture.publish(Baseline::Absent, &three()),
            Outcome::Rejected(_)
        ));
        assert_eq!(fs::read(fixture.record()).unwrap(), b"damaged");
    }

    #[test]
    fn temporaries_are_ignored_by_reads_and_removed_before_each_write() {
        let fixture = Fixture::new();
        let bytes = three().encode().unwrap();
        let torn = format!("{TEMPORARY}{}", "ab".repeat(16));
        fixture.write(&torn, bytes.get(..bytes.len() / 2).unwrap());
        fixture.write("tmp-whole", &bytes);
        fixture.write("tmp-", b"");
        fs::write(fixture.root.join("victim"), b"outside").unwrap();
        symlink(fixture.root.join("victim"), fixture.dir.join("tmp-link")).unwrap();
        let kept = [
            ".tmp-1",
            "1001",
            "TMP-1",
            "cutover-reboot",
            "tmp",
            "tmp_1",
            "xtmp-1",
        ];
        for name in kept {
            fixture.write(name, name.as_bytes());
        }
        assert_eq!(fixture.state(), Seen::Unenrolled);
        fixture.seed(&two());
        assert_eq!(fixture.state(), enrolled(&two()));
        let baseline = fixture.baseline();
        assert_eq!(fixture.publish(baseline, &three()), Outcome::Committed);
        let mut expected: Vec<&str> = kept.to_vec();
        expected.push("1000");
        expected.sort_unstable();
        assert_eq!(fixture.names(), expected);
        for name in kept {
            assert_eq!(fs::read(fixture.dir.join(name)).unwrap(), name.as_bytes());
        }
        assert_eq!(fs::read(fixture.root.join("victim")).unwrap(), b"outside");
        // Removal cleans them as well.
        fixture.write(&torn, b"torn");
        let baseline = fixture.baseline();
        assert_eq!(fixture.store().remove(baseline), Outcome::Committed);
        assert_eq!(fixture.state(), Seen::Unenrolled);
        assert!(!fixture.names().contains(&torn));
        // A prefixed name that cannot be unlinked refuses the write.
        fs::create_dir(fixture.dir.join("tmp-dir")).unwrap();
        assert!(fixture.store().remove_temporaries().is_err());
        assert!(matches!(
            fixture.publish(Baseline::Absent, &three()),
            Outcome::Rejected(_)
        ));
        assert_eq!(fixture.state(), Seen::Unenrolled);
        fs::remove_dir(fixture.dir.join("tmp-dir")).unwrap();
        assert_eq!(
            fixture.publish(Baseline::Absent, &three()),
            Outcome::Committed
        );
    }

    #[test]
    fn a_displaced_temporary_is_left_alone() {
        let fixture = Fixture::new();
        let temporary = fixture.dir.join(format!("{TEMPORARY}{}", "08".repeat(16)));
        let outcome = fixture.store().publish_inner(
            Baseline::Absent,
            &three(),
            &mut io::repeat(8),
            &mut |stage| {
                if stage == Stage::FileSynced {
                    fs::remove_file(&temporary).unwrap();
                    fixture.write(temporary.file_name().unwrap().to_str().unwrap(), b"x");
                }
                Ok(())
            },
        );
        assert!(matches!(outcome, Outcome::Rejected(_)));
        assert_eq!(fs::read(&temporary).unwrap(), b"x");
        assert_eq!(fixture.state(), Seen::Unenrolled);
    }

    #[test]
    fn a_lost_or_replaced_proc_is_unreadable_never_absent() {
        let fixture = Fixture::new();
        let owner = fixture.owner();
        let unreadable = Seen::Unavailable(Cause::Unreadable);
        let lost = fixture.root.join("lost");
        assert_eq!(
            Store::open_via(&lost, &fixture.dir, owner, UID).err(),
            Some(Cause::Unreadable)
        );
        let mut store = fixture.store();
        assert_eq!(seen(store.read()), Seen::Unenrolled);
        store.directory.set_fd_root(lost);
        assert_eq!(seen(store.read()), unreadable);
        // A stand-in tree where the descriptor's path names another directory.
        let fake = fixture.root.join("fake");
        let shadow = fake.join(store.directory.file().as_raw_fd().to_string());
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&shadow)
            .unwrap();
        fs::write(shadow.join("tmp-kept"), b"kept").unwrap();
        store.directory.set_fd_root(fake);
        assert_eq!(seen(store.read()), unreadable);
        fixture.seed(&three());
        assert_eq!(seen(store.read()), unreadable);
        assert!(store.remove_temporaries().is_err());
        assert!(matches!(
            store.publish(Baseline::Absent, &two(), &mut io::repeat(1)),
            Outcome::Rejected(_)
        ));
        assert_eq!(fs::read(shadow.join("tmp-kept")).unwrap(), b"kept");
        store
            .directory
            .set_fd_root(PathBuf::from(crate::login_state::FD_ROOT));
        assert_eq!(seen(store.read()), enrolled(&three()));
    }

    #[test]
    fn a_record_changing_under_a_read_is_unreadable() {
        let fixture = Fixture::new();
        let record = fixture.record();
        let cases: &[(ReadStage, &dyn Fn(), Seen)] = &[
            // Another valid record swapped in between inspection and open.
            (
                ReadStage::Inspected,
                &|| {
                    fixture.write("swap", &two().encode().unwrap());
                    fs::rename(fixture.dir.join("swap"), &record).unwrap();
                },
                enrolled(&two()),
            ),
            // The same inode changed mode between inspection and open.
            (
                ReadStage::Inspected,
                &|| fs::set_permissions(&record, Permissions::from_mode(0o640)).unwrap(),
                Seen::Unavailable(Cause::RecordDamaged),
            ),
            // Bytes appended while the record was read.
            (
                ReadStage::Read,
                &|| {
                    OpenOptions::new()
                        .append(true)
                        .open(&record)
                        .and_then(|mut file| file.write_all(&[0]))
                        .unwrap()
                },
                Seen::Unavailable(Cause::RecordDamaged),
            ),
        ];
        for (index, (stage, change, settled)) in cases.iter().enumerate() {
            let _ = fs::remove_file(&record);
            fixture.seed(&three());
            let store = fixture.store();
            let result = store.read_record_inner(&mut |point| {
                if point == *stage {
                    change();
                }
            });
            assert!(matches!(result, Err(Cause::Unreadable)), "{index}");
            // A fresh read shows the state the change settled on.
            assert_eq!(seen(store.read()), *settled, "{index}");
        }
    }
}
