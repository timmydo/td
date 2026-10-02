//! Safe fixture for the dedicated allocation-measurement process.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use std::{os::unix::fs::DirBuilderExt, path::PathBuf};

struct Fixture {
    path: PathBuf,
    root: LockedRoot,
    account: AccountId,
}
impl Fixture {
    fn new(path: PathBuf) -> Self {
        create_directory(&path);
        let path = fs::canonicalize(path).unwrap();
        let directory = Directory::from_path(path.to_str().unwrap()).unwrap();
        let owner = directory.metadata().unwrap().uid();
        let lock = super::super::acquire_lock(&directory, owner).unwrap();
        // Only this cfg(test) fixture bypasses deployment root admission.
        // The actual lock and every temporary-file I/O check still run.
        let root = LockedRoot {
            root: super::super::PrivateRoot { directory },
            _lock: lock,
        };
        let account = AccountId::from_bytes([0x42; 16]);
        let name = Name::account(account, AccountEntry::Temporary).unwrap();
        let mut parent = path.clone();
        for part in name.as_path().unwrap().components() {
            parent.push(part);
            create_directory(&parent);
        }
        for entry in [
            AccountEntry::Messages,
            AccountEntry::Shard(crate::format::row::BlobKind::Message, 0xff),
        ] {
            root.create_account_directory(account, entry).unwrap();
        }
        Self {
            path,
            root,
            account,
        }
    }
}
fn create_directory(path: &Path) {
    fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Invokes the snapshot callback immediately before and after just the measured
/// operations. Fixture preparation and removal are deliberately outside it.
pub fn run(mut snapshot: impl FnMut()) {
    let base = std::env::temp_dir().join(format!("td-mta-file-alloc-{}", std::process::id()));
    create_directory(&base);
    let base = fs::canonicalize(base).unwrap();
    let tail = super::super::MAX_ROOT_BYTES
        .checked_sub(base.as_os_str().len())
        .and_then(|remaining| remaining.checked_sub(1))
        .filter(|length| (1..=255).contains(length))
        .expect("allocation fixture TMPDIR must leave a valid maximum root component");
    let short = Fixture::new(base.join("short"));
    let long = Fixture::new(base.join("x".repeat(tail)));
    assert_eq!(long.path.as_os_str().len(), super::super::MAX_ROOT_BYTES);
    snapshot();
    for fixture in [&short, &long] {
        super::publication::probe(&fixture.root, fixture.account);
        assert!(
            matches!(fixture.root.create_accounts_directory(), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::AlreadyExists)
        );
        let directory_account = AccountId::from_bytes([0x17; 16]);
        for entry in [
            AccountEntry::Root,
            AccountEntry::Metadata,
            AccountEntry::Checkpoints,
            AccountEntry::Checkpoint(Number::new(u64::MAX).unwrap()),
            AccountEntry::Temporary,
            AccountEntry::Messages,
            AccountEntry::Shard(crate::format::row::BlobKind::Message, 0xff),
        ] {
            let directory = fixture
                .root
                .create_account_directory(directory_account, entry)
                .unwrap();
            assert!(directory.metadata().unwrap().is_dir());
            drop(directory);
            assert!(
                matches!(fixture.root.create_account_directory(directory_account, entry), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::AlreadyExists)
            );
        }
        assert!(
            matches!(fixture.root.create_account_directory(directory_account, AccountEntry::Current), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::InvalidInput)
        );
        for number in 1..=16 {
            let number = Number::new(number).unwrap();
            let mut file = fixture
                .root
                .create_temporary(fixture.account, number, 4096)
                .unwrap();
            file.write(&[0x5a; 4096]).unwrap();
            assert_eq!(
                file.write(b"!").unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(
                matches!(fixture.root.create_temporary(fixture.account, number, 4096), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::AlreadyExists)
            );
            let synced = file.sync().unwrap();
            let mut bytes = [0; 4096];
            assert_eq!(synced.read_at(0, &mut bytes).unwrap(), 4096);
            assert!(bytes.iter().all(|byte| *byte == 0x5a));
            assert_eq!(synced.read_at(4096, &mut bytes).unwrap(), 0);
            assert_eq!(
                synced.read_at(4097, &mut bytes).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            drop(synced);
        }
        assert!(
            matches!(fixture.root.create_temporary(AccountId::from_bytes([1;16]), Number::new(1).unwrap(), 1), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::NotFound)
        );
        let file = fixture
            .root
            .create_temporary(fixture.account, Number::new(17).unwrap(), 1)
            .unwrap();
        assert_eq!(
            file.sync_using(|_| Err(io::ErrorKind::StorageFull.into()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::StorageFull
        );
        for number in 18..=19 {
            let name = Name::account(
                fixture.account,
                AccountEntry::TemporaryFile(Number::new(number).unwrap()),
            )
            .unwrap();
            let result = create_using(
                &fixture.root.root.directory,
                &name,
                |path| {
                    let file = open_new(path)?;
                    if number == 18 {
                        return Err(io::ErrorKind::PermissionDenied.into());
                    }
                    Ok(file)
                },
                |_, _| Err(io::ErrorKind::StorageFull.into()),
            );
            if number == 18 {
                assert!(
                    matches!(result, Err(CreateError::Attempted(e)) if e.kind() == io::ErrorKind::PermissionDenied)
                );
            } else {
                assert!(
                    matches!(result, Err(CreateError::Created(e)) if e.kind() == io::ErrorKind::StorageFull)
                );
            }
        }
        let mut file = fixture
            .root
            .create_temporary(fixture.account, Number::new(20).unwrap(), 1)
            .unwrap();
        assert_eq!(
            file.progress
                .write(&mut Interrupted, b"x")
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            file.write(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(file.sync().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        let mut file = fixture
            .root
            .create_temporary(fixture.account, Number::new(21).unwrap(), 1)
            .unwrap();
        file.write(b"x").unwrap();
        let file = file.sync().unwrap();
        file.file.file.set_len(0).unwrap();
        assert_eq!(
            file.read_at(0, &mut [0]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
    snapshot();
    drop(short);
    drop(long);
    fs::remove_dir(base).unwrap();
}

struct Interrupted;
impl Write for Interrupted {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::Interrupted.into())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
