//! Private output and consuming immutable publication under the writer lock.
#[path = "temporary/publication.rs"]
mod publication;
pub use publication::{PublishError, PublishedFile};
#[cfg(test)]
#[path = "temporary/probe.rs"]
pub mod probe;

use super::{Directory, LockedRoot, MAX_PATH_BYTES};
use crate::{
    ids::AccountId,
    store_paths::{AccountEntry, Name, Number},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

/// Maximum caller data processed before returning to the worker's meter.
pub const MAX_FILE_STEP_BYTES: usize = 64 * 1024;
const MAX_WRITE_CALLS: usize = 64;

/// Whether this attempt created an inode determines orphan accounting.
#[derive(Debug)]
pub enum CreateError {
    Uncreated(io::Error),
    Attempted(io::Error),
    Created(io::Error),
}
impl std::fmt::Display for CreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Uncreated(_) => f.write_str("private storage object was not created"),
            Self::Attempted(_) => f.write_str("private storage creation has uncertain effects"),
            Self::Created(_) => f.write_str("private storage object requires orphan accounting"),
        }
    }
}
impl std::error::Error for CreateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Uncreated(e) | Self::Attempted(e) | Self::Created(e) => e,
        })
    }
}

impl LockedRoot {
    /// Low-level I/O: the caller must admit/charge the account, file and byte
    /// limit first. This does not establish reservation or account authority.
    pub fn create_temporary(
        &self,
        account: AccountId,
        number: Number,
        limit: u64,
    ) -> Result<TemporaryFile<'_>, CreateError> {
        if limit > i64::MAX as u64 {
            return Err(CreateError::Uncreated(io::ErrorKind::InvalidInput.into()));
        }
        let name = Name::account(account, AccountEntry::TemporaryFile(number))
            .map_err(|_| CreateError::Uncreated(io::ErrorKind::InvalidInput.into()))?;
        let (file, parent) = create(&self.root.directory, &name)?;
        Ok(TemporaryFile {
            _owner: self,
            account,
            file,
            parent,
            name,
            progress: Progress {
                length: 0,
                limit,
                failed: false,
            },
        })
    }
}

/// Sequential bounded output, no clone/raw handle/unlink escape. Drop closes
/// descriptors and leaves the file charged for explicit cleanup/recovery.
#[derive(Debug)]
pub struct TemporaryFile<'a> {
    _owner: &'a LockedRoot,
    account: AccountId,
    file: File,
    parent: File,
    name: Name,
    progress: Progress,
}
impl<'a> TemporaryFile<'a> {
    pub fn name(&self) -> &Name {
        &self.name
    }
    /// Confirmed bytes; a failed write can have additional uncertain effects.
    pub fn len(&self) -> u64 {
        self.progress.length
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Authoritative after any write error: error kinds alone cannot distinguish
    /// pre-I/O refusal from the same kind returned by the filesystem.
    pub fn is_failed(&self) -> bool {
        self.progress.failed
    }
    /// Refuse capacity or a chunk over MAX_FILE_STEP_BYTES before I/O. At most
    /// 64 write attempts; exhaustion returns WouldBlock and retires output.
    /// Any I/O failure retires this output; callers must not replay the chunk,
    /// sync it as complete or release its charge. Check is_failed after errors.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.progress.write(&mut self.file, bytes)
    }
    /// Consumes the writable owner. File and temporary-parent sync are both
    /// required; no destination has been published and no commit is promised.
    pub fn sync(self) -> io::Result<SyncedTemporary<'a>> {
        self.sync_using(File::sync_all)
    }
}

#[derive(Debug)]
pub struct SyncedTemporary<'a> {
    file: TemporaryFile<'a>,
}
impl SyncedTemporary<'_> {
    pub fn name(&self) -> &Name {
        self.file.name()
    }
    pub fn len(&self) -> u64 {
        self.file.len()
    }
    pub fn is_empty(&self) -> bool {
        self.file.is_empty()
    }
    /// Reads at most the caller's slice and the completed private extent.
    /// Short reads/Interrupted propagate; zero before extent end is corruption.
    pub fn read_at(&self, offset: u64, output: &mut [u8]) -> io::Result<usize> {
        read_extent(&self.file.file, self.len(), offset, output)
    }
}

fn read_extent(file: &File, length: u64, offset: u64, output: &mut [u8]) -> io::Result<usize> {
    let remaining = length
        .checked_sub(offset)
        .ok_or(io::ErrorKind::InvalidInput)?;
    let count = usize::try_from(remaining)
        .unwrap_or(usize::MAX)
        .min(output.len())
        .min(MAX_FILE_STEP_BYTES);
    if count == 0 {
        return Ok(0);
    }
    let count = file.read_at(
        output.get_mut(..count).ok_or(io::ErrorKind::InvalidInput)?,
        offset,
    )?;
    if count == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(count)
}

impl<'a> TemporaryFile<'a> {
    fn sync_using(
        self,
        mut sync: impl FnMut(&File) -> io::Result<()>,
    ) -> io::Result<SyncedTemporary<'a>> {
        if self.progress.failed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if self.file.metadata()?.len() != self.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        sync(&self.file)?;
        sync(&self.parent)?;
        Ok(SyncedTemporary { file: self })
    }
}

#[derive(Debug)]
struct Progress {
    length: u64,
    limit: u64,
    failed: bool,
}
impl Progress {
    fn write(&mut self, file: &mut impl Write, mut bytes: &[u8]) -> io::Result<()> {
        if self.failed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if bytes.len() > MAX_FILE_STEP_BYTES {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let count = u64::try_from(bytes.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
        let end = self
            .length
            .checked_add(count)
            .filter(|end| *end <= self.limit)
            .ok_or(io::ErrorKind::InvalidInput)?;
        self.failed = true;
        let mut calls = 0;
        while !bytes.is_empty() {
            if calls == MAX_WRITE_CALLS {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            calls += 1;
            match file.write(bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => {
                    bytes = bytes.get(count..).ok_or(io::ErrorKind::InvalidData)?;
                    self.length = self
                        .length
                        .checked_add(u64::try_from(count).map_err(|_| io::ErrorKind::InvalidData)?)
                        .ok_or(io::ErrorKind::InvalidData)?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
                Err(error) => return Err(error),
            }
        }
        if self.length != end {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.failed = false;
        Ok(())
    }
}

fn create(root: &Directory, name: &Name) -> Result<(File, File), CreateError> {
    create_using(root, name, open_new, prepare)
}
fn create_using(
    root: &Directory,
    name: &Name,
    open: impl FnOnce(&Path) -> io::Result<File>,
    prepare: impl FnOnce(&File, u32) -> io::Result<()>,
) -> Result<(File, File), CreateError> {
    let mut buffer = [0; MAX_PATH_BYTES];
    let destination = root
        .destination(name, &mut buffer)
        .map_err(CreateError::Uncreated)?;
    super::require_absent(destination.path).map_err(CreateError::Uncreated)?;
    // Once open is issued, an error need not prove that no inode was created.
    let file = open(destination.path).map_err(CreateError::Attempted)?;
    prepare(&file, destination.owner).map_err(CreateError::Created)?;
    Ok((file, destination.parent.file))
}
fn open_new(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}
fn prepare(file: &File, owner: u32) -> io::Result<()> {
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != 0
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        os::unix::fs::{symlink, DirBuilderExt},
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct Fixture {
        path: PathBuf,
        root: LockedRoot,
        account: AccountId,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "td-mta-temp-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            let path = fs::canonicalize(path).unwrap();
            let directory = Directory::from_path(path.to_str().unwrap()).unwrap();
            let owner = directory.metadata().unwrap().uid();
            let lock = super::super::acquire_lock(&directory, owner).unwrap();
            // Exercise I/O under the harness identity, without relaxing the
            // separate production root-admission preconditions.
            let root = LockedRoot {
                root: super::super::PrivateRoot { directory },
                _lock: lock,
            };
            let account = AccountId::from_bytes([9; 16]);
            let temporary = Name::account(account, AccountEntry::Temporary).unwrap();
            let mut parent = path.clone();
            for part in temporary.as_path().unwrap().components() {
                parent.push(part);
                fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
                fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self {
                path,
                root,
                account,
            }
        }
        fn create(&self, number: u64, limit: u64) -> Result<TemporaryFile<'_>, CreateError> {
            self.root
                .create_temporary(self.account, Number::new(number).unwrap(), limit)
        }
        fn path(&self, number: u64) -> PathBuf {
            self.path.join(
                Name::account(
                    self.account,
                    AccountEntry::TemporaryFile(Number::new(number).unwrap()),
                )
                .unwrap()
                .as_path()
                .unwrap(),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn exclusive_creation_and_capacity_refusal_preserve_bytes() {
        let fixture = Fixture::new();
        let mut output = fixture.create(1, 5).unwrap();
        assert!(output.is_empty());
        assert_eq!(
            fs::metadata(fixture.path(1)).unwrap().mode() & 0o7777,
            0o600
        );
        output.write(b"abc").unwrap();
        assert_eq!(
            output.write(b"def").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!output.is_failed());
        assert_eq!(output.len(), 3);
        assert_eq!(fs::read(fixture.path(1)).unwrap(), b"abc");
        assert!(
            matches!(fixture.create(1, 5), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::AlreadyExists)
        );
        output.write(b"de").unwrap();
        let synced = output.sync().unwrap();
        assert_eq!(synced.len(), 5);
        assert_eq!(
            synced.name().as_str().unwrap(),
            "accounts/09090909090909090909090909090909/tmp/00000000000000000001.tmp"
        );
        let mut bytes = [0xa5; 8];
        assert_eq!(synced.read_at(1, &mut bytes).unwrap(), 4);
        assert_eq!(bytes, [b'b', b'c', b'd', b'e', 0xa5, 0xa5, 0xa5, 0xa5]);
        assert_eq!(synced.read_at(5, &mut bytes).unwrap(), 0);
        assert_eq!(
            synced.read_at(6, &mut bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(synced.read_at(1, &mut []).unwrap(), 0);
        drop(synced);
        assert_eq!(fs::read(fixture.path(1)).unwrap(), b"abcde");
        assert!(
            matches!(fixture.create(2, u64::MAX), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::InvalidInput)
        );
        assert!(!fixture.path(2).exists());
        drop(fixture.create(3, i64::MAX as u64).unwrap());
        assert!(
            matches!(fixture.create(4, i64::MAX as u64 + 1), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::InvalidInput)
        );
        assert!(!fixture.path(4).exists());
        let mut empty = fixture.create(2, 0).unwrap();
        empty.write(b"").unwrap();
        assert!(empty.write(b"x").is_err());
        assert!(empty.sync().unwrap().is_empty());
    }

    #[test]
    fn paths_refuse_existing_links_and_nonprivate_ancestors() {
        let fixture = Fixture::new();
        fs::write(fixture.path.join("outside"), b"untouched").unwrap();
        symlink(fixture.path.join("outside"), fixture.path(1)).unwrap();
        assert!(
            matches!(fixture.create(1, 5), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::AlreadyExists)
        );
        assert_eq!(
            fs::read(fixture.path.join("outside")).unwrap(),
            b"untouched"
        );
        let accounts = fixture.path.join("accounts");
        let account = accounts.join(fixture.account.to_string());
        for path in [&fixture.path, &accounts, &account, &account.join("tmp")] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o750)).unwrap();
            assert!(
                matches!(fixture.create(2, 5), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::PermissionDenied)
            );
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let saved = fixture.path.join("saved");
        fs::rename(&accounts, &saved).unwrap();
        symlink(&saved, &accounts).unwrap();
        assert!(
            matches!(fixture.create(2, 5), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::PermissionDenied)
        );
        fs::remove_file(&accounts).unwrap();
        assert!(
            matches!(fixture.create(2, 5), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn post_create_failures_leave_an_accountable_inode() {
        let fixture = Fixture::new();
        for number in 1..=2 {
            let name = Name::account(
                fixture.account,
                AccountEntry::TemporaryFile(Number::new(number).unwrap()),
            )
            .unwrap();
            let result = create_using(
                &fixture.root.root.directory,
                &name,
                open_new,
                |file, owner| {
                    if number == 2 {
                        prepare(file, owner)?;
                    }
                    Err(io::ErrorKind::StorageFull.into())
                },
            );
            assert!(
                matches!(result, Err(CreateError::Created(e)) if e.kind() == io::ErrorKind::StorageFull)
            );
            assert_eq!(fs::metadata(fixture.path(number)).unwrap().len(), 0);
            assert!(
                matches!(fixture.create(number, 1), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::AlreadyExists)
            );
        }
        let name = Name::account(
            fixture.account,
            AccountEntry::TemporaryFile(Number::new(3).unwrap()),
        )
        .unwrap();
        let result = create_using(
            &fixture.root.root.directory,
            &name,
            |path| {
                drop(open_new(path)?);
                Err(io::ErrorKind::PermissionDenied.into())
            },
            prepare,
        );
        assert!(
            matches!(result, Err(CreateError::Attempted(e)) if e.kind() == io::ErrorKind::PermissionDenied)
        );
        assert_eq!(fs::metadata(fixture.path(3)).unwrap().len(), 0);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(fixture.path(3))
            .unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o400))
            .unwrap();
        prepare(&file, file.metadata().unwrap().uid()).unwrap();
        assert_eq!(file.metadata().unwrap().mode() & 0o7777, 0o600);
    }

    #[test]
    fn sync_failures_before_and_after_each_barrier_leave_private_output() {
        let fixture = Fixture::new();
        for failure in 0..4 {
            let mut output = fixture.create(failure + 1, 5).unwrap();
            output.write(b"hello").unwrap();
            let mut step = 0;
            let result = output.sync_using(|file| {
                if step == failure {
                    return Err(io::ErrorKind::StorageFull.into());
                }
                step += 1;
                file.sync_all()?;
                if step == failure {
                    return Err(io::ErrorKind::StorageFull.into());
                }
                step += 1;
                Ok(())
            });
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::StorageFull);
            assert_eq!(fs::read(fixture.path(failure + 1)).unwrap(), b"hello");
        }
        let output = fixture.create(5, 1).unwrap();
        fs::write(fixture.path(5), b"x").unwrap();
        assert_eq!(
            output.sync().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut output = fixture.create(6, 3).unwrap();
        output.write(b"abc").unwrap();
        let synced = output.sync().unwrap();
        fs::write(fixture.path(6), b"").unwrap();
        assert_eq!(
            synced.read_at(0, &mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let mut output = fixture.create(7, 3).unwrap();
        let mut sink = script([Step::Bytes(1), Step::Error(io::ErrorKind::StorageFull)]);
        assert!(output.progress.write(&mut sink, b"abc").is_err());
        let mut called = false;
        let result = output.sync_using(|_| {
            called = true;
            Ok(())
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert!(!called);
        assert!(fixture.path(7).exists());
    }

    enum Step {
        Bytes(usize),
        Error(io::ErrorKind),
    }
    struct Script {
        steps: VecDeque<Step>,
        bytes: Vec<u8>,
        calls: usize,
    }
    impl Write for Script {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            match self.steps.pop_front().unwrap() {
                Step::Bytes(count) => {
                    if let Some(part) = bytes.get(..count) {
                        self.bytes.extend_from_slice(part);
                    }
                    Ok(count)
                }
                Step::Error(error) => Err(error.into()),
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn script(steps: impl IntoIterator<Item = Step>) -> Script {
        Script {
            steps: steps.into_iter().collect(),
            bytes: Vec::new(),
            calls: 0,
        }
    }

    #[test]
    fn step_limits_bound_chunks_interruptions_and_short_writes() {
        let mut progress = Progress {
            length: 0,
            limit: u64::MAX,
            failed: false,
        };
        let mut sink = script([]);
        assert_eq!(
            progress
                .write(&mut sink, &[0; MAX_FILE_STEP_BYTES + 1])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(sink.calls, 0);
        assert!(!progress.failed);
        let mut sink = script(
            std::iter::repeat_with(|| Step::Error(io::ErrorKind::Interrupted))
                .take(MAX_WRITE_CALLS + 1),
        );
        assert_eq!(
            progress.write(&mut sink, b"x").unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(sink.calls, MAX_WRITE_CALLS);
        assert_eq!(progress.length, 0);
        assert!(progress.failed);
        let mut progress = Progress {
            length: 0,
            limit: u64::MAX,
            failed: false,
        };
        let mut sink = script(std::iter::repeat_with(|| Step::Bytes(1)).take(MAX_WRITE_CALLS + 1));
        assert_eq!(
            progress
                .write(&mut sink, &[0; MAX_WRITE_CALLS + 1])
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(sink.calls, MAX_WRITE_CALLS);
        assert_eq!(progress.length, MAX_WRITE_CALLS as u64);
        assert!(progress.failed);
        let fixture = Fixture::new();
        let mut file = fixture.create(1, (MAX_FILE_STEP_BYTES + 1) as u64).unwrap();
        file.write(&[1; MAX_FILE_STEP_BYTES]).unwrap();
        file.write(b"x").unwrap();
        let file = file.sync().unwrap();
        let mut bytes = [0; MAX_FILE_STEP_BYTES + 1];
        assert_eq!(file.read_at(0, &mut bytes).unwrap(), MAX_FILE_STEP_BYTES);
        assert_eq!(bytes.last(), Some(&0));
    }

    #[test]
    fn partial_interrupted_zero_and_failed_writes_keep_exact_progress() {
        let mut progress = Progress {
            length: 0,
            limit: 5,
            failed: false,
        };
        let mut sink = script([
            Step::Error(io::ErrorKind::Interrupted),
            Step::Bytes(2),
            Step::Bytes(3),
        ]);
        progress.write(&mut sink, b"hello").unwrap();
        assert_eq!(sink.bytes, b"hello");
        assert_eq!(progress.length, 5);
        assert!(!progress.failed);
        assert!(progress.write(&mut sink, b"!").is_err());
        assert_eq!(sink.calls, 3);
        for ending in [
            Step::Bytes(0),
            Step::Error(io::ErrorKind::StorageFull),
            Step::Error(io::ErrorKind::InvalidInput),
            Step::Bytes(50),
        ] {
            let mut progress = Progress {
                length: 0,
                limit: 5,
                failed: false,
            };
            let mut sink = script([Step::Bytes(2), ending]);
            assert!(progress.write(&mut sink, b"hello").is_err());
            assert_eq!(progress.length, 2);
            assert!(progress.failed);
            assert_eq!(
                progress.write(&mut sink, b"hello").unwrap_err().kind(),
                io::ErrorKind::BrokenPipe
            );
            assert_eq!(sink.calls, 2);
            assert_eq!(sink.bytes, b"he");
        }
        let mut progress = Progress {
            length: u64::MAX,
            limit: u64::MAX,
            failed: false,
        };
        let mut sink = script([]);
        assert!(progress.write(&mut sink, b"!").is_err());
        assert_eq!(sink.calls, 0);
        assert!(!progress.failed);
    }
}
