//! Expected-selector replacement using a synced same-directory temporary file.
use super::*;
use crate::{
    format::{
        container::{Current, Error as ContainerError},
        CURRENT_BYTES,
    },
    ports::Crypto,
};

const MAX_CURRENT_READ_CALLS: usize = 64;

/// Encoded intent only; the caller must validate and durably publish the entire
/// selected graph, admit the work and hold the actual writer/view barrier.
#[derive(Debug)]
pub struct CurrentUpdate {
    account: AccountId,
    previous: Option<[u8; CURRENT_BYTES]>,
    next: [u8; CURRENT_BYTES],
}
impl CurrentUpdate {
    pub fn prepare(
        crypto: &impl Crypto,
        previous: Option<Current>,
        next: Current,
    ) -> Result<Self, ContainerError> {
        if let Some(previous) = previous {
            if previous.account != next.account
                || previous.epoch != next.epoch
                || previous.generation >= next.generation
            {
                return Err(crate::format::Error::InvalidValue.into());
            }
        }
        let mut next_bytes = [0; CURRENT_BYTES];
        next.encode(crypto, &mut next_bytes)?;
        let previous = if let Some(previous) = previous {
            let mut bytes = [0; CURRENT_BYTES];
            previous.encode(crypto, &mut bytes)?;
            Some(bytes)
        } else {
            None
        };
        Ok(Self {
            account: next.account,
            previous,
            next: next_bytes,
        })
    }
}

#[derive(Debug)]
pub enum CurrentError {
    Rejected(io::Error),
    Create(CreateError),
    Private(io::Error),
    RenameAttempted(io::Error),
    Renamed(io::Error),
}
impl std::fmt::Display for CurrentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Rejected(_) => "CURRENT update refused before temporary creation",
            Self::Create(_) => "CURRENT temporary creation failed",
            Self::Private(_) => "CURRENT private output failed before rename",
            Self::RenameAttempted(_) => "CURRENT rename has uncertain effects",
            Self::Renamed(_) => "CURRENT renamed but parent sync failed",
        })
    }
}
impl std::error::Error for CurrentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Create(e) => Some(e),
            Self::Rejected(e) | Self::Private(e) | Self::RenameAttempted(e) | Self::Renamed(e) => {
                Some(e)
            }
        }
    }
}

impl LockedRoot {
    /// Serialize with every store mutation and hold the checkpoint barrier.
    /// Both rename error stages require writer stop and selected-store recovery.
    pub fn replace_current(
        &self,
        update: &CurrentUpdate,
        number: Number,
    ) -> Result<(), CurrentError> {
        self.replace_current_using(update, number, Real)
    }
    fn replace_current_using(
        &self,
        update: &CurrentUpdate,
        number: Number,
        mut ops: impl Operations,
    ) -> Result<(), CurrentError> {
        let root = &self.root.directory;
        let name = Name::account(update.account, AccountEntry::Current)
            .map_err(|_| CurrentError::Rejected(io::ErrorKind::InvalidInput.into()))?;
        let mut target_buffer = [0; MAX_PATH_BYTES];
        let target = root
            .destination(&name, &mut target_buffer)
            .map_err(CurrentError::Rejected)?;
        check_expected(target.path, target.owner, update.previous.as_ref())
            .map_err(CurrentError::Rejected)?;
        let name = Name::account(update.account, AccountEntry::CurrentTemporary(number))
            .map_err(|_| CurrentError::Rejected(io::ErrorKind::InvalidInput.into()))?;
        let mut file = self
            .create_named(update.account, name, CURRENT_BYTES as u64)
            .map_err(CurrentError::Create)?;
        file.write(&update.next).map_err(CurrentError::Private)?;
        let file = file
            .sync_using(|file| ops.sync(file))
            .map_err(CurrentError::Private)?;
        let mut source_buffer = [0; MAX_PATH_BYTES];
        let source = root
            .destination(file.name(), &mut source_buffer)
            .map_err(CurrentError::Private)?;
        super::publication::validate_source(&file.file, source.path, target.owner)
            .map_err(CurrentError::Private)?;
        let source_parent = source.parent.metadata().map_err(CurrentError::Private)?;
        if !super::super::same_file(
            &source_parent,
            &file.file.parent.metadata().map_err(CurrentError::Private)?,
        ) || !super::super::same_file(
            &source_parent,
            &target.parent.metadata().map_err(CurrentError::Private)?,
        ) {
            return Err(CurrentError::Private(io::ErrorKind::InvalidData.into()));
        }
        ops.rename(source.path, target.path)
            .map_err(CurrentError::RenameAttempted)?;
        ops.sync(&target.parent.file)
            .map_err(CurrentError::Renamed)?;
        Ok(())
    }
}

fn check_expected(
    path: &Path,
    owner: u32,
    expected: Option<&[u8; CURRENT_BYTES]>,
) -> io::Result<()> {
    let Some(expected) = expected else {
        return super::super::require_absent(path);
    };
    let before = fs::symlink_metadata(path)?;
    check_file(&before, owner)?;
    let file = File::open(path)?;
    let after = file.metadata()?;
    check_file(&after, owner)?;
    if !super::super::same_file(&before, &after) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut bytes = [0; CURRENT_BYTES];
    let mut offset = 0;
    let mut calls = 0;
    while offset < bytes.len() {
        if calls == MAX_CURRENT_READ_CALLS {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        calls += 1;
        let output = bytes.get_mut(offset..).ok_or(io::ErrorKind::InvalidData)?;
        match file.read_at(output, offset as u64) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => {
                offset = offset
                    .checked_add(count)
                    .ok_or(io::ErrorKind::InvalidData)?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
            Err(e) => return Err(e),
        }
    }
    if file.read_at(&mut [0; 1], CURRENT_BYTES as u64)? != 0 || &bytes != expected {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}
fn check_file(metadata: &fs::Metadata, owner: u32) -> io::Result<()> {
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != CURRENT_BYTES as u64
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}
trait Operations {
    fn rename(&mut self, source: &Path, target: &Path) -> io::Result<()>;
    fn sync(&mut self, file: &File) -> io::Result<()>;
}
struct Real;
impl Operations for Real {
    fn rename(&mut self, source: &Path, target: &Path) -> io::Result<()> {
        fs::rename(source, target)
    }
    fn sync(&mut self, file: &File) -> io::Result<()> {
        file.sync_all()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) fn probe(root: &LockedRoot, account: AccountId) {
    let first = tests::selector(account, 1);
    let initial = CurrentUpdate::prepare(&td_crypto::Provider, None, first).unwrap();
    root.replace_current(&initial, Number::new(300).unwrap())
        .unwrap();
    assert!(
        matches!(root.replace_current(&initial, Number::new(301).unwrap()), Err(CurrentError::Rejected(e)) if e.kind() == io::ErrorKind::AlreadyExists)
    );
    let next = CurrentUpdate::prepare(
        &td_crypto::Provider,
        Some(first),
        tests::selector(account, 2),
    )
    .unwrap();
    root.replace_current(&next, Number::new(301).unwrap())
        .unwrap();
    assert!(
        matches!(root.replace_current(&next, Number::new(302).unwrap()), Err(CurrentError::Rejected(e)) if e.kind() == io::ErrorKind::InvalidData)
    );
    for point in 0..8 {
        let account = AccountId::from_bytes([0x60 + point as u8; 16]);
        let first = tests::selector(account, 1);
        let initial = CurrentUpdate::prepare(&td_crypto::Provider, None, first).unwrap();
        root.replace_current(&initial, Number::new(1).unwrap())
            .unwrap();
        let next = CurrentUpdate::prepare(
            &td_crypto::Provider,
            Some(first),
            tests::selector(account, 2),
        )
        .unwrap();
        let fault = tests::Fault {
            point,
            step: 0,
            root,
            account,
            number: Number::new(2).unwrap(),
        };
        let result = root.replace_current_using(&next, Number::new(2).unwrap(), fault);
        assert!(matches!(
            (point, result),
            (0..=3, Err(CurrentError::Private(_)))
                | (4..=5, Err(CurrentError::RenameAttempted(_)))
                | (6..=7, Err(CurrentError::Renamed(_)))
        ));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::super::tests::Fixture;
    use super::*;
    use crate::ids::StoreEpoch;
    use std::os::unix::fs::symlink;
    const ACCOUNT: AccountId = AccountId::from_bytes([4; 16]);
    pub(super) fn selector(account: AccountId, generation: u64) -> Current {
        Current {
            account,
            epoch: StoreEpoch::from_bytes([1; 16]),
            generation,
            manifest_digest: [generation as u8; 32],
        }
    }
    fn setup(root: &LockedRoot) {
        root.create_accounts_directory().unwrap();
        root.create_account_directory(ACCOUNT, AccountEntry::Root)
            .unwrap();
        root.create_account_directory(ACCOUNT, AccountEntry::Metadata)
            .unwrap();
    }
    fn path(fixture: &Fixture, entry: AccountEntry) -> std::path::PathBuf {
        fixture
            .path
            .join(Name::account(ACCOUNT, entry).unwrap().as_path().unwrap())
    }
    fn update(previous: Option<Current>, next: u64) -> CurrentUpdate {
        CurrentUpdate::prepare(&td_crypto::Provider, previous, selector(ACCOUNT, next)).unwrap()
    }
    #[test]
    fn current_replacement_checks_previous_and_preserves_valid_selectors() {
        let fixture = Fixture::new();
        let root = fixture.locked();
        setup(&root);
        let current_path = path(&fixture, AccountEntry::Current);
        let first = update(None, 1);
        root.replace_current(&first, Number::new(u64::MAX).unwrap())
            .unwrap();
        assert_eq!(
            Current::decode(&td_crypto::Provider, &fs::read(&current_path).unwrap()).unwrap(),
            selector(ACCOUNT, 1)
        );
        let temp_name = Name::account(
            ACCOUNT,
            AccountEntry::CurrentTemporary(Number::new(u64::MAX).unwrap()),
        )
        .unwrap();
        assert_eq!(
            temp_name.as_str().unwrap(),
            "accounts/04040404040404040404040404040404/metadata/CURRENT.18446744073709551615.tmp"
        );
        assert!(!fixture.path.join(temp_name.as_path().unwrap()).exists());
        assert!(
            matches!(root.replace_current(&first, Number::new(2).unwrap()), Err(CurrentError::Rejected(e)) if e.kind()==io::ErrorKind::AlreadyExists)
        );
        let old_file = File::open(&current_path).unwrap();
        let next = update(Some(selector(ACCOUNT, 1)), 2);
        root.replace_current(&next, Number::new(2).unwrap())
            .unwrap();
        let mut old_bytes = [0; CURRENT_BYTES];
        assert_eq!(old_file.read_at(&mut old_bytes, 0).unwrap(), CURRENT_BYTES);
        assert_eq!(
            Current::decode(&td_crypto::Provider, &old_bytes).unwrap(),
            selector(ACCOUNT, 1)
        );
        assert_eq!(
            Current::decode(&td_crypto::Provider, &fs::read(&current_path).unwrap()).unwrap(),
            selector(ACCOUNT, 2)
        );
        assert_eq!(fs::metadata(&current_path).unwrap().mode() & 0o7777, 0o600);
        assert_eq!(fs::metadata(&current_path).unwrap().nlink(), 1);
        assert!(
            matches!(root.replace_current(&next, Number::new(3).unwrap()), Err(CurrentError::Rejected(e)) if e.kind()==io::ErrorKind::InvalidData)
        );
        assert!(!path(
            &fixture,
            AccountEntry::CurrentTemporary(Number::new(3).unwrap())
        )
        .exists());
        fs::remove_file(&current_path).unwrap();
        assert!(
            matches!(root.replace_current(&next, Number::new(3).unwrap()), Err(CurrentError::Rejected(e)) if e.kind()==io::ErrorKind::NotFound)
        );
        assert!(!current_path.exists());
    }

    #[test]
    fn current_intent_refuses_cross_account_epoch_and_nonadvancing_generations() {
        let previous = selector(ACCOUNT, 2);
        for next in [
            selector(ACCOUNT, 0),
            selector(ACCOUNT, 1),
            selector(ACCOUNT, 2),
            selector(AccountId::from_bytes([9; 16]), 3),
            Current {
                epoch: StoreEpoch::from_bytes([2; 16]),
                ..selector(ACCOUNT, 3)
            },
        ] {
            assert!(CurrentUpdate::prepare(&td_crypto::Provider, Some(previous), next).is_err());
        }
        assert!(CurrentUpdate::prepare(&td_crypto::Provider, None, selector(ACCOUNT, 0)).is_err());
    }

    #[test]
    fn current_policy_and_temporary_collisions_preserve_old_state() {
        for case in 0..6 {
            let fixture = Fixture::new();
            let root = fixture.locked();
            setup(&root);
            let first = update(None, 1);
            root.replace_current(&first, Number::new(1).unwrap())
                .unwrap();
            let current = path(&fixture, AccountEntry::Current);
            match case {
                0 => fs::set_permissions(&current, fs::Permissions::from_mode(0o640)).unwrap(),
                1 => fs::hard_link(&current, fixture.path.join("extra")).unwrap(),
                2 => fs::write(&current, b"short").unwrap(),
                3 => fs::write(&current, [0; CURRENT_BYTES]).unwrap(),
                4 => {
                    let mut bytes = first.next;
                    *bytes.last_mut().unwrap() ^= 1;
                    fs::write(&current, bytes).unwrap();
                }
                _ => {
                    fs::remove_file(&current).unwrap();
                    symlink("missing", &current).unwrap();
                }
            }
            let before = fs::symlink_metadata(&current).unwrap();
            let contents = before.is_file().then(|| fs::read(&current).unwrap());
            let next = update(Some(selector(ACCOUNT, 1)), 2);
            assert!(matches!(
                root.replace_current(&next, Number::new(2).unwrap()),
                Err(CurrentError::Rejected(_))
            ));
            assert!(!path(
                &fixture,
                AccountEntry::CurrentTemporary(Number::new(2).unwrap())
            )
            .exists());
            let after = fs::symlink_metadata(&current).unwrap();
            assert_eq!(
                (
                    before.dev(),
                    before.ino(),
                    before.mode(),
                    before.nlink(),
                    before.len()
                ),
                (
                    after.dev(),
                    after.ino(),
                    after.mode(),
                    after.nlink(),
                    after.len()
                )
            );
            if let Some(contents) = contents {
                assert_eq!(fs::read(&current).unwrap(), contents);
            } else {
                assert_eq!(fs::read_link(&current).unwrap(), Path::new("missing"));
            }
        }
        let fixture = Fixture::new();
        let root = fixture.locked();
        setup(&root);
        let first = update(None, 1);
        root.replace_current(&first, Number::new(1).unwrap())
            .unwrap();
        let temporary = path(
            &fixture,
            AccountEntry::CurrentTemporary(Number::new(2).unwrap()),
        );
        fs::write(&temporary, b"leftover").unwrap();
        assert!(
            matches!(root.replace_current(&update(Some(selector(ACCOUNT, 1)), 2), Number::new(2).unwrap()), Err(CurrentError::Create(CreateError::Uncreated(e))) if e.kind()==io::ErrorKind::AlreadyExists)
        );
        assert_eq!(fs::read(&temporary).unwrap(), b"leftover");
        assert_eq!(
            fs::read(path(&fixture, AccountEntry::Current)).unwrap(),
            first.next
        );
    }

    #[test]
    fn current_replacement_refuses_a_swapped_metadata_directory() {
        struct Swap {
            directory: std::path::PathBuf,
            saved: std::path::PathBuf,
            temporary: std::ffi::OsString,
            syncs: usize,
        }
        impl Operations for Swap {
            fn sync(&mut self, file: &File) -> io::Result<()> {
                file.sync_all()?;
                self.syncs += 1;
                if self.syncs == 2 {
                    fs::rename(&self.directory, &self.saved)?;
                    fs::create_dir(&self.directory)?;
                    fs::set_permissions(&self.directory, fs::Permissions::from_mode(0o700))?;
                    fs::rename(
                        self.saved.join(&self.temporary),
                        self.directory.join(&self.temporary),
                    )?;
                }
                Ok(())
            }
            fn rename(&mut self, source: &Path, target: &Path) -> io::Result<()> {
                fs::rename(source, target)
            }
        }
        let fixture = Fixture::new();
        let root = fixture.locked();
        setup(&root);
        let first = update(None, 1);
        root.replace_current(&first, Number::new(1).unwrap())
            .unwrap();
        let number = Number::new(2).unwrap();
        let temporary = path(&fixture, AccountEntry::CurrentTemporary(number));
        let saved = fixture.path.join("saved-metadata");
        let next = update(Some(selector(ACCOUNT, 1)), 2);
        let result = root.replace_current_using(
            &next,
            number,
            Swap {
                directory: temporary.parent().unwrap().to_owned(),
                saved: saved.clone(),
                temporary: temporary.file_name().unwrap().to_owned(),
                syncs: 0,
            },
        );
        assert!(
            matches!(result, Err(CurrentError::Private(e)) if e.kind() == io::ErrorKind::InvalidData)
        );
        assert!(!path(&fixture, AccountEntry::Current).exists());
        assert_eq!(fs::read(saved.join("CURRENT")).unwrap(), first.next);
        assert_eq!(fs::read(&temporary).unwrap(), next.next);
        assert!(!saved.join(temporary.file_name().unwrap()).exists());
    }

    pub(super) struct Fault<'a> {
        pub(super) point: usize,
        pub(super) step: usize,
        pub(super) root: &'a LockedRoot,
        pub(super) account: AccountId,
        pub(super) number: Number,
    }
    impl Fault<'_> {
        fn perform(&mut self, operation: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
            if self.step == self.point {
                return Err(io::ErrorKind::StorageFull.into());
            }
            self.step += 1;
            operation()?;
            if self.step == self.point {
                return Err(io::ErrorKind::StorageFull.into());
            }
            self.step += 1;
            Ok(())
        }
    }
    impl Operations for Fault<'_> {
        fn sync(&mut self, file: &File) -> io::Result<()> {
            assert!(matches!(self.step, 0 | 2 | 6));
            let entry = if self.step == 0 {
                AccountEntry::CurrentTemporary(self.number)
            } else {
                AccountEntry::Metadata
            };
            let name = Name::account(self.account, entry).unwrap();
            let mut buffer = [0; MAX_PATH_BYTES];
            let expected =
                fs::metadata(self.root.root.directory.join(&name, &mut buffer)?).unwrap();
            let actual = file.metadata()?;
            assert_eq!(
                (actual.dev(), actual.ino()),
                (expected.dev(), expected.ino())
            );
            self.perform(|| file.sync_all())
        }
        fn rename(&mut self, source: &Path, target: &Path) -> io::Result<()> {
            assert_eq!(self.step, 4);
            assert_eq!(source.parent(), target.parent());
            self.perform(|| fs::rename(source, target))
        }
    }

    #[test]
    fn current_sync_and_rename_failures_require_exact_recovery_stages() {
        for point in 0..8 {
            let fixture = Fixture::new();
            let root = fixture.locked();
            setup(&root);
            let first = update(None, 1);
            root.replace_current(&first, Number::new(1).unwrap())
                .unwrap();
            let next = update(Some(selector(ACCOUNT, 1)), 2);
            let number = Number::new(2).unwrap();
            let result = root.replace_current_using(
                &next,
                number,
                Fault {
                    point,
                    step: 0,
                    root: &root,
                    account: ACCOUNT,
                    number,
                },
            );
            assert!(matches!((point, result),
                (0..=3, Err(CurrentError::Private(e)))
                | (4..=5, Err(CurrentError::RenameAttempted(e)))
                | (6..=7, Err(CurrentError::Renamed(e))) if e.kind() == io::ErrorKind::StorageFull));
            let actual = fs::read(path(&fixture, AccountEntry::Current)).unwrap();
            assert_eq!(actual, if point < 5 { first.next } else { next.next });
            let temp = path(&fixture, AccountEntry::CurrentTemporary(number));
            assert_eq!(temp.exists(), point < 5);
            if temp.exists() {
                assert_eq!(fs::read(temp).unwrap(), next.next);
            }
        }
    }
}
