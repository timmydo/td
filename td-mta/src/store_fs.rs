//! Bounded std filesystem access within operator-controlled, stable paths.
use crate::store_paths::Name;
use std::{
    fs::{self, File, Metadata},
    io,
    os::unix::fs::MetadataExt,
    path::Path,
};

#[path = "store_fs/create_directory.rs"]
mod create_directory;
#[path = "store_fs/input.rs"]
mod input;
pub use input::{
    CompleteFile, CompletePrefix, PrefixReader, RecoveryInput, RecoveryInputError, RepairError,
    RepairedJournal, ScannedJournal, StoreReader,
};
#[path = "store_fs/selection.rs"]
mod selection;
pub use selection::{SelectionError, SelectionScratch, SelectionStage};
#[path = "store_fs/blob.rs"]
mod blob;
pub use blob::{BlobInput, BlobInputError, CompleteBlob};
#[path = "store_fs/table.rs"]
mod table;
pub use table::{
    CompleteLookup, CompleteNext, CompleteReplay, CompleteTable, LookupError, NextError,
    TableInput, TableInputError, TableLookup, TableNext, TableReplay, TableReplayError,
};
#[path = "store_fs/active_overlay.rs"]
mod active_overlay;
pub use active_overlay::{LoadedOverlay, OverlayInputError};
#[path = "store_fs/active.rs"]
mod active;
#[path = "store_fs/journal_input.rs"]
mod journal_input;
pub use active::{ActiveInput, ActiveInputError, CompleteActive};
#[path = "store_fs/history.rs"]
mod history;
pub use history::{CompleteHistory, HistoryInput, HistoryInputError};
#[path = "store_fs/active_changes.rs"]
mod active_changes;
#[path = "store_fs/change_input.rs"]
mod change_input;
pub use active_changes::{ActiveChangesInput, CompleteActiveChanges};
#[path = "store_fs/history_changes.rs"]
mod history_changes;
pub use history_changes::{CompleteHistoryChanges, HistoryChangesInput};
#[path = "store_fs/temporary.rs"]
mod temporary;
pub use temporary::{
    CreateError, CurrentError, CurrentUpdate, MetadataDestination, PublishError, PublishedFile,
    SyncedTemporary, TemporaryFile, MAX_FILE_STEP_BYTES,
};

#[cfg(test)]
pub use temporary::probe::run as probe_temporary_io;

/// Qualified by the host and portable allocation probes, including errors.
/// This is a service limit, not a promise about every Rust implementation.
pub const MAX_PATH_BYTES: usize = 383;
pub const MAX_ROOT_BYTES: usize = MAX_PATH_BYTES - crate::store_paths::CAPACITY - 1;

#[derive(Debug)]
pub enum RootError {
    Path,
    Io(io::Error),
    Owner,
    WritableAncestor,
    PrivateMode,
}
impl std::fmt::Display for RootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path => f.write_str("invalid or overlong storage path"),
            Self::Io(error) => write!(f, "storage directory operation: {error}"),
            Self::Owner => f.write_str("data root owner must be non-root and ancestors trusted"),
            Self::WritableAncestor => f.write_str("data root ancestor permits shared writes"),
            Self::PrivateMode => f.write_str("data root requires mode 0700 without special bits"),
        }
    }
}
impl std::error::Error for RootError {}
impl From<io::Error> for RootError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Startup checks under the deployment's stable-path assumption. The supervisor
/// must run as this directory's dedicated owner; std does not verify process UID.
/// Root/owner/mount authority and all concurrent namespace writers are trusted.
#[derive(Debug)]
pub struct PrivateRoot {
    directory: Directory,
}
impl PrivateRoot {
    pub fn open(path: &str) -> Result<Self, RootError> {
        if path.len() > MAX_ROOT_BYTES {
            return Err(RootError::Path);
        }
        validate(path).map_err(|_| RootError::Path)?;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() {
            return Err(RootError::Io(io::ErrorKind::NotADirectory.into()));
        }
        if metadata.mode() & 0o7777 != 0o700 {
            return Err(RootError::PrivateMode);
        }
        let owner = metadata.uid();
        if owner == 0 {
            return Err(RootError::Owner);
        }
        for ancestor in Path::new(path).ancestors().skip(1) {
            let metadata = fs::symlink_metadata(ancestor)?;
            if !metadata.is_dir() {
                return Err(RootError::Io(io::ErrorKind::NotADirectory.into()));
            }
            if !trusted_owner(metadata.uid(), owner) {
                return Err(RootError::Owner);
            }
            if metadata.mode() & 0o022 != 0 {
                return Err(RootError::WritableAncestor);
            }
        }
        let directory = Directory::from_path(path)?;
        if !same_file(&metadata, &directory.metadata()?) {
            return Err(RootError::Io(io::ErrorKind::InvalidData.into()));
        }
        Ok(Self { directory })
    }
    /// Startup-only acquisition. Retain the returned owner for the entire
    /// writer lifetime; opening a checked root alone never excludes writers.
    pub fn try_lock(self) -> Result<LockedRoot, LockError> {
        let owner = self.directory.metadata()?.uid();
        let lock = acquire_lock(&self.directory, owner)?;
        Ok(LockedRoot {
            root: self,
            _lock: lock,
        })
    }
    pub fn directory(&self) -> &Directory {
        &self.directory
    }
}

/// A retained File for metadata/sync and a fixed pathname for future lookup.
/// New lookups use that pathname, so renames or mount changes during service
/// operation are unsupported. This is not a directory-confinement capability.
#[derive(Debug)]
pub struct Directory {
    file: File,
    path: [u8; MAX_PATH_BYTES],
    length: usize,
}
impl Directory {
    /// Checks each existing component for directory type (rejecting symlinks).
    /// Checks are not atomic with open and do not defend against hostile races.
    /// No ownership, permission or writer-lock authority is established here.
    pub fn from_path(path: &str) -> io::Result<Self> {
        validate(path)?;
        for ancestor in Path::new(path).ancestors() {
            if !fs::symlink_metadata(ancestor)?.is_dir() {
                return Err(io::ErrorKind::NotADirectory.into());
            }
        }
        let before = fs::symlink_metadata(path)?;
        let file = File::open(path)?;
        let after = file.metadata()?;
        if !after.is_dir() || !same_file(&before, &after) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut bytes = [0; MAX_PATH_BYTES];
        bytes
            .get_mut(..path.len())
            .ok_or(io::ErrorKind::InvalidInput)?
            .copy_from_slice(path.as_bytes());
        Ok(Self {
            file,
            path: bytes,
            length: path.len(),
        })
    }

    /// A generated name is relative to this stored path. Keep every ancestor
    /// stable while serving; this lookup does not follow the retained File.
    pub fn open(&self, name: &Name) -> io::Result<Self> {
        let mut path = [0; MAX_PATH_BYTES];
        let path = self.join(name, &mut path)?;
        Self::from_path(path.to_str().ok_or(io::ErrorKind::InvalidInput)?)
    }
    fn join<'a>(&self, name: &Name, path: &'a mut [u8; MAX_PATH_BYTES]) -> io::Result<&'a Path> {
        let mut output = crate::bounded::TextBuffer::new(path);
        let root = std::str::from_utf8(
            self.path
                .get(..self.length)
                .ok_or(io::ErrorKind::InvalidInput)?,
        )
        .map_err(|_| io::ErrorKind::InvalidInput)?;
        let name = name.as_str().map_err(|_| io::ErrorKind::InvalidInput)?;
        let separator = if root == "/" { "" } else { "/" };
        output
            .format(format_args!("{root}{separator}{name}"))
            .map_err(|_| io::ErrorKind::InvalidInput)?;
        let length = output.len();
        let text = std::str::from_utf8(path.get(..length).ok_or(io::ErrorKind::InvalidInput)?)
            .map_err(|_| io::ErrorKind::InvalidInput)?;
        Ok(Path::new(text))
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        self.file.metadata()
    }
    fn destination<'a>(
        &self,
        name: &Name,
        buffer: &'a mut [u8; MAX_PATH_BYTES],
    ) -> io::Result<Destination<'a>> {
        let path = self.join(name, buffer)?;
        let parent_path = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let owner = self.metadata()?.uid();
        for ancestor in parent_path.ancestors() {
            if ancestor.as_os_str().len() < self.length {
                break;
            }
            private_directory(ancestor, owner)?;
        }
        let parent =
            Directory::from_path(parent_path.to_str().ok_or(io::ErrorKind::InvalidInput)?)?;
        Ok(Destination {
            path,
            parent,
            owner,
        })
    }
}
struct Destination<'a> {
    path: &'a Path,
    parent: Directory,
    owner: u32,
}
fn private_directory(path: &Path, owner: u32) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o7777 != 0o700 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}
fn require_absent(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(io::ErrorKind::AlreadyExists.into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
/// Cooperative process exclusion only; store-format recovery is still required.
#[derive(Debug)]
pub struct LockedRoot {
    root: PrivateRoot,
    _lock: File,
}
impl LockedRoot {
    pub fn root(&self) -> &PrivateRoot {
        &self.root
    }
}
#[derive(Debug)]
pub enum LockError {
    Busy,
    Policy,
    Io(io::Error),
}
impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("mail store writer lock is held"),
            Self::Policy => f.write_str(
                "LOCK must be a private empty regular file owned by the store owner with one link",
            ),
            Self::Io(error) => write!(f, "mail store writer lock: {error}"),
        }
    }
}
impl std::error::Error for LockError {}
impl From<io::Error> for LockError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
fn check_lock(metadata: &Metadata, owner: u32) -> Result<(), LockError> {
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != 0
    {
        return Err(LockError::Policy);
    }
    Ok(())
}
fn acquire_lock(directory: &Directory, owner: u32) -> Result<File, LockError> {
    use std::{
        fs::OpenOptions,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    // Cold startup path. No clone or unlock operation escapes the lock owner.
    let name = Name::root(crate::store_paths::RootEntry::Lock)
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut buffer = [0; MAX_PATH_BYTES];
    let path = directory.join(&name, &mut buffer)?;
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => {
            // Restore owner bits filtered by umask, never grant shared access.
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let before = fs::symlink_metadata(path)?;
            check_lock(&before, owner)?;
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            if !same_file(&before, &file.metadata()?) {
                return Err(LockError::Policy);
            }
            file
        }
        Err(error) => return Err(error.into()),
    };
    check_lock(&file.metadata()?, owner)?;
    match file.try_lock() {
        Ok(()) => (),
        Err(fs::TryLockError::WouldBlock) => return Err(LockError::Busy),
        Err(fs::TryLockError::Error(error)) => return Err(error.into()),
    }
    file.sync_all()?;
    directory.file.sync_all()?;
    Ok(file)
}

fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}
fn validate(path: &str) -> io::Result<()> {
    if path.len() > MAX_PATH_BYTES {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    crate::config::values::absolute_path(path).map_err(|_| io::ErrorKind::InvalidInput.into())
}

fn trusted_owner(owner: u32, service: u32) -> bool {
    owner == 0 || owner == service
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, Write},
        os::unix::fs::{DirBuilderExt, PermissionsExt},
        path::PathBuf,
        process::{Child, Command, Stdio},
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant},
    };

    pub(super) struct Fixture {
        pub(super) path: PathBuf,
        directory: Directory,
        owner: u32,
    }
    impl Fixture {
        pub(super) fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "td-mta-lock-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            let path = fs::canonicalize(path).unwrap();
            let directory = Directory::from_path(path.to_str().unwrap()).unwrap();
            let owner = directory.metadata().unwrap().uid();
            Self {
                path,
                directory,
                owner,
            }
        }
        fn lock(&self) -> Result<File, LockError> {
            acquire_lock(&self.directory, self.owner)
        }
        pub(super) fn locked(&self) -> LockedRoot {
            let directory = Directory::from_path(self.path.to_str().unwrap()).unwrap();
            let lock = acquire_lock(&directory, self.owner).unwrap();
            LockedRoot {
                root: PrivateRoot { directory },
                _lock: lock,
            }
        }
        fn reacquire(&self) -> File {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match self.lock() {
                    Err(LockError::Busy) if Instant::now() < deadline => {
                        // A concurrent spawn can retain the file until exec.
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    result => return result.unwrap(),
                }
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn persistent_lock_contends_and_reopens_same_inode() {
        let fixture = Fixture::new();
        let file = fixture.lock().unwrap();
        let metadata = file.metadata().unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(metadata.len(), 0);
        assert!(matches!(fixture.lock(), Err(LockError::Busy)));
        drop(file);
        let again = fixture.reacquire();
        assert!(same_file(&metadata, &again.metadata().unwrap()));
        assert!(fixture.path.join("LOCK").exists());
    }

    #[test]
    fn lock_policy_refuses_existing_links_types_owners_modes_and_contents() {
        let fixture = Fixture::new();
        let path = fixture.path.join("LOCK");
        let file = fixture.lock().unwrap();
        let metadata = file.metadata().unwrap();
        assert!(matches!(
            check_lock(&metadata, fixture.owner ^ 1),
            Err(LockError::Policy)
        ));
        drop(file);
        for mode in [0o644, 0o660, 0o400, 0o1600, 0o2600, 0o4600] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(matches!(fixture.lock(), Err(LockError::Policy)));
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, b"not a lock file").unwrap();
        assert!(matches!(fixture.lock(), Err(LockError::Policy)));
        assert_eq!(fs::read(&path).unwrap(), b"not a lock file");
        fs::write(&path, b"").unwrap();
        let other = fixture.path.join("other");
        fs::hard_link(&path, &other).unwrap();
        assert!(matches!(fixture.lock(), Err(LockError::Policy)));
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&other, &path).unwrap();
        assert!(matches!(fixture.lock(), Err(LockError::Policy)));
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(matches!(fixture.lock(), Err(LockError::Policy)));
    }

    #[test]
    fn lock_creation_permission_failure_retains_io_cause() {
        let fixture = Fixture::new();
        fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o500)).unwrap();
        let result = fixture.lock();
        fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o700)).unwrap();
        match result {
            Err(LockError::Io(error)) => assert_eq!(error.kind(), io::ErrorKind::PermissionDenied),
            Ok(_) => {
                eprintln!("permission-negative fixture unavailable: identity bypasses mode 0500")
            }
            other => assert!(matches!(other, Err(LockError::Io(_))), "{other:?}"),
        }
        let missing = Fixture::new();
        fs::remove_dir(&missing.path).unwrap();
        assert!(
            matches!(missing.lock(), Err(LockError::Io(error)) if error.kind() == io::ErrorKind::NotFound)
        );
    }

    #[test]
    #[ignore = "child process helper; parent requires its explicit ready marker"]
    fn lock_process_child() {
        let path = std::env::var("TD_MTA_LOCK_FIXTURE").unwrap();
        let directory = Directory::from_path(&path).unwrap();
        let owner = directory.metadata().unwrap().uid();
        let _lock = acquire_lock(&directory, owner).unwrap();
        println!("td-mta-lock-ready");
        std::io::stdout().flush().unwrap();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
    }

    #[test]
    fn independent_process_lock_is_released_after_process_death() {
        let fixture = Fixture::new();
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "store_fs::tests::lock_process_child",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("TD_MTA_LOCK_FIXTURE", &fixture.path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let output = child.0.stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut output = std::io::BufReader::new(output);
            let mut line = String::new();
            loop {
                line.clear();
                match output.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) if line.trim_end().ends_with("td-mta-lock-ready") => {
                        let _ = sender.send(());
                        return;
                    }
                    Ok(_) => (),
                }
            }
        });
        let ready = receiver.recv_timeout(Duration::from_secs(5));
        if ready.is_err() {
            // Unblock the pipe reader before joining it on this failure path.
            let _ = child.0.kill();
            let _ = child.0.wait();
        }
        reader.join().unwrap();
        ready.unwrap();
        assert!(matches!(fixture.lock(), Err(LockError::Busy)));
        let before = fs::metadata(fixture.path.join("LOCK")).unwrap();
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let acquired = fixture.reacquire();
        assert!(same_file(&before, &acquired.metadata().unwrap()));
    }

    #[test]
    fn ancestor_owner_policy_excludes_other_identities() {
        assert!(super::trusted_owner(0, 1000));
        assert!(super::trusted_owner(1000, 1000));
        assert!(!super::trusted_owner(1001, 1000));
        assert!(!super::trusted_owner(u32::MAX, 1000));
    }
}
