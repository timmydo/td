//! Non-replacing file publication; no metadata commit or admission authority.
use super::*;
use crate::{
    format::{row::BlobKind, Table},
    ids::BlobId,
};

/// Fresh unselected metadata only. CURRENT cannot be named by this type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataDestination {
    Table(Number, Table),
    Manifest(Number),
    Journal(Number),
}
impl MetadataDestination {
    fn entry(self) -> AccountEntry {
        match self {
            Self::Table(generation, table) => AccountEntry::Table(generation, table),
            Self::Manifest(generation) => AccountEntry::Manifest(generation),
            Self::Journal(segment) => AccountEntry::Journal(segment),
        }
    }
}

/// Last established boundary, not proof of absence after a failed mutation.
/// Every failure retains logical charges until explicit cleanup or recovery.
#[derive(Debug)]
pub enum PublishError {
    Rejected(io::Error),
    LinkAttempted(io::Error),
    Linked(io::Error),
    DestinationSynced(io::Error),
    TemporaryUnlinked(io::Error),
}
impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Rejected(_) => "file publication refused before linking",
            Self::LinkAttempted(_) => "file link has uncertain effects",
            Self::Linked(_) => "file linked but destination sync failed",
            Self::DestinationSynced(_) => "file destination synced but temporary unlink failed",
            Self::TemporaryUnlinked(_) => "file temporary removed but parent sync failed",
        })
    }
}
impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Rejected(e)
            | Self::LinkAttempted(e)
            | Self::Linked(e)
            | Self::DestinationSynced(e)
            | Self::TemporaryUnlinked(e) => e,
        })
    }
}

/// Retains a read-only API and writer-lock borrow. This low-level result is not
/// ports::PublishedBlob: no quota, digest or transaction authority is established.
#[derive(Debug)]
pub struct PublishedFile<'a> {
    _owner: &'a LockedRoot,
    file: File,
    name: Name,
    length: u64,
}
impl PublishedFile<'_> {
    pub fn name(&self) -> &Name {
        &self.name
    }
    pub fn len(&self) -> u64 {
        self.length
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub fn read_at(&self, offset: u64, output: &mut [u8]) -> io::Result<usize> {
        read_extent(&self.file, self.length, offset, output)
    }
}

impl<'a> SyncedTemporary<'a> {
    /// Publish in the account bound at creation. The caller admits the operation
    /// and verifies the body/digest first. Existing destinations are never adopted.
    pub fn publish_blob(
        self,
        kind: BlobKind,
        id: BlobId,
    ) -> Result<PublishedFile<'a>, PublishError> {
        self.publish_using(kind, id, Real)
    }
    /// Publish a fresh table, manifest or initial journal into pre-existing
    /// durable parents. The caller checks format/digest and selected reachability;
    /// publication neither selects a generation nor grants journal append access.
    pub fn publish_metadata(
        self,
        destination: MetadataDestination,
    ) -> Result<PublishedFile<'a>, PublishError> {
        self.publish_entry_using(destination.entry(), Real)
    }
    fn publish_using(
        self,
        kind: BlobKind,
        id: BlobId,
        ops: impl Operations,
    ) -> Result<PublishedFile<'a>, PublishError> {
        self.publish_entry_using(AccountEntry::Blob(kind, id), ops)
    }
    fn publish_entry_using(
        self,
        entry: AccountEntry,
        mut ops: impl Operations,
    ) -> Result<PublishedFile<'a>, PublishError> {
        let output = self.file;
        let root = &output._owner.root.directory;
        let name = Name::account(output.account, entry)
            .map_err(|_| PublishError::Rejected(io::ErrorKind::InvalidInput.into()))?;
        let mut source_buffer = [0; MAX_PATH_BYTES];
        let source = root
            .destination(&output.name, &mut source_buffer)
            .map_err(PublishError::Rejected)?;
        let mut target_buffer = [0; MAX_PATH_BYTES];
        let target = root
            .destination(&name, &mut target_buffer)
            .map_err(PublishError::Rejected)?;
        validate_source(&output, source.path, source.owner).map_err(PublishError::Rejected)?;
        if !super::super::same_file(
            &source.parent.metadata().map_err(PublishError::Rejected)?,
            &output.parent.metadata().map_err(PublishError::Rejected)?,
        ) {
            return Err(PublishError::Rejected(io::ErrorKind::InvalidData.into()));
        }
        super::super::require_absent(target.path).map_err(PublishError::Rejected)?;
        ops.link(source.path, target.path)
            .map_err(PublishError::LinkAttempted)?;
        ops.sync(&target.parent.file)
            .map_err(PublishError::Linked)?;
        ops.unlink(source.path)
            .map_err(PublishError::DestinationSynced)?;
        ops.sync(&output.parent)
            .map_err(PublishError::TemporaryUnlinked)?;
        Ok(PublishedFile {
            _owner: output._owner,
            file: output.file,
            name,
            length: output.progress.length,
        })
    }
}

fn validate_source(output: &TemporaryFile<'_>, path: &Path, owner: u32) -> io::Result<()> {
    let named = fs::symlink_metadata(path)?;
    let held = output.file.metadata()?;
    for metadata in [&named, &held] {
        if !metadata.is_file()
            || metadata.mode() & 0o7777 != 0o600
            || metadata.uid() != owner
            || metadata.nlink() != 1
            || metadata.len() != output.len()
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
    }
    if !super::super::same_file(&named, &held) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

trait Operations {
    fn link(&mut self, source: &Path, target: &Path) -> io::Result<()>;
    fn sync(&mut self, file: &File) -> io::Result<()>;
    fn unlink(&mut self, path: &Path) -> io::Result<()>;
}
struct Real;
impl Operations for Real {
    fn link(&mut self, source: &Path, target: &Path) -> io::Result<()> {
        fs::hard_link(source, target)
    }
    fn sync(&mut self, file: &File) -> io::Result<()> {
        file.sync_all()
    }
    fn unlink(&mut self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) fn probe(root: &LockedRoot, account: AccountId) {
    let generation = Number::new(u64::MAX).unwrap();
    for (index, destination) in [
        MetadataDestination::Table(generation, Table::ThreadAnchors),
        MetadataDestination::Manifest(generation),
        MetadataDestination::Journal(generation),
    ]
    .into_iter()
    .enumerate()
    {
        for collision in [false, true] {
            let mut file = root
                .create_temporary(account, Number::new(200 + index as u64).unwrap(), 5)
                .unwrap();
            file.write(b"hello").unwrap();
            let result = file.sync().unwrap().publish_metadata(destination);
            if collision {
                assert!(
                    matches!(result, Err(PublishError::Rejected(e)) if e.kind() == io::ErrorKind::AlreadyExists)
                );
            } else {
                let file = result.unwrap();
                assert_eq!(
                    file.name(),
                    &Name::account(account, destination.entry()).unwrap()
                );
                let mut output = [0; 5];
                assert_eq!(file.read_at(0, &mut output).unwrap(), 5);
                assert_eq!(&output, b"hello");
            }
        }
    }
    let mut bytes = [0xff; 16];
    for point in 0..10 {
        *bytes.last_mut().unwrap() = point.min(8) as u8;
        let id = BlobId::from_bytes(bytes);
        let mut file = root
            .create_temporary(account, Number::new(100 + point).unwrap(), 5)
            .unwrap();
        file.write(b"hello").unwrap();
        let file = file.sync().unwrap();
        if point < 8 {
            let name = Name::account(account, AccountEntry::Blob(BlobKind::Message, id)).unwrap();
            let mut buffer = [0; MAX_PATH_BYTES];
            let target = root.root.directory.destination(&name, &mut buffer).unwrap();
            let fault = tests::Fault {
                point: point as usize,
                step: 0,
                target: target.parent.metadata().unwrap(),
                source: file.file.parent.metadata().unwrap(),
            };
            let error = file
                .publish_using(BlobKind::Message, id, fault)
                .unwrap_err();
            assert!(matches!(
                (point, error),
                (0..=1, PublishError::LinkAttempted(_))
                    | (2..=3, PublishError::Linked(_))
                    | (4..=5, PublishError::DestinationSynced(_))
                    | (6..=7, PublishError::TemporaryUnlinked(_))
            ));
        } else if point == 8 {
            let published = file.publish_blob(BlobKind::Message, id).unwrap();
            let mut output = [0; 8];
            assert_eq!(published.read_at(0, &mut output).unwrap(), 5);
            assert_eq!(output.get(..5), Some(b"hello".as_slice()));
            assert_eq!(published.read_at(5, &mut output).unwrap(), 0);
            assert_eq!(
                published.read_at(6, &mut output).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        } else {
            assert!(
                matches!(file.publish_blob(BlobKind::Message, id), Err(PublishError::Rejected(e)) if e.kind() == io::ErrorKind::AlreadyExists)
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::super::super::tests::Fixture;
    use super::*;
    use std::os::unix::fs::symlink;

    const ACCOUNT: AccountId = AccountId::from_bytes([3; 16]);
    const BLOB: BlobId = BlobId::from_bytes([0xff; 16]);

    fn setup(root: &LockedRoot) {
        root.create_accounts_directory().unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Temporary,
            AccountEntry::Messages,
            AccountEntry::Uploads,
            AccountEntry::Shard(BlobKind::Message, 0xff),
            AccountEntry::Shard(BlobKind::Upload, 0xff),
        ] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
    }
    fn temporary(root: &LockedRoot, number: u64) -> SyncedTemporary<'_> {
        let mut file = root
            .create_temporary(ACCOUNT, Number::new(number).unwrap(), 5)
            .unwrap();
        file.write(b"hello").unwrap();
        file.sync().unwrap()
    }
    fn path(fixture: &Fixture, entry: AccountEntry) -> std::path::PathBuf {
        fixture
            .path
            .join(Name::account(ACCOUNT, entry).unwrap().as_path().unwrap())
    }

    #[test]
    fn publication_is_exclusive_durable_and_account_bound() {
        let fixture = Fixture::new();
        let root = fixture.locked();
        setup(&root);
        for (index, kind) in [BlobKind::Message, BlobKind::Upload]
            .into_iter()
            .enumerate()
        {
            let number = index as u64 + 1;
            let source = path(
                &fixture,
                AccountEntry::TemporaryFile(Number::new(number).unwrap()),
            );
            let target = path(&fixture, AccountEntry::Blob(kind, BLOB));
            let published = temporary(&root, number).publish_blob(kind, BLOB).unwrap();
            assert_eq!(
                published.name(),
                &Name::account(ACCOUNT, AccountEntry::Blob(kind, BLOB)).unwrap()
            );
            assert_eq!(published.len(), 5);
            assert!(!published.is_empty());
            let mut bytes = [0xa5; 8];
            assert_eq!(published.read_at(1, &mut bytes).unwrap(), 4);
            assert_eq!(bytes, [b'e', b'l', b'l', b'o', 0xa5, 0xa5, 0xa5, 0xa5]);
            assert_eq!(published.read_at(5, &mut bytes).unwrap(), 0);
            assert_eq!(
                published.read_at(6, &mut bytes).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(!source.exists());
            let metadata = fs::metadata(&target).unwrap();
            assert_eq!((metadata.mode() & 0o7777, metadata.nlink()), (0o600, 1));
            assert_eq!(
                metadata.uid(),
                root.root.directory.metadata().unwrap().uid()
            );
            drop(published);
            fs::write(&target, b"prior").unwrap();
            let error = temporary(&root, number)
                .publish_blob(kind, BLOB)
                .unwrap_err();
            assert!(
                matches!(error, PublishError::Rejected(e) if e.kind() == io::ErrorKind::AlreadyExists)
            );
            assert_eq!(fs::read(&target).unwrap(), b"prior");
            assert_eq!(fs::read(&source).unwrap(), b"hello");
        }
    }

    #[test]
    fn metadata_names_are_exclusive_and_do_not_select_current() {
        let fixture = Fixture::new();
        let root = fixture.locked();
        setup(&root);
        let generation = Number::new(u64::MAX).unwrap();
        for entry in [
            AccountEntry::Metadata,
            AccountEntry::Checkpoints,
            AccountEntry::Checkpoint(generation),
            AccountEntry::Journals,
        ] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
        let checkpoint =
            "accounts/03030303030303030303030303030303/metadata/checkpoints/18446744073709551615";
        let tables = [
            (Table::Blobs, "blobs.tbl"),
            (Table::Mailboxes, "mailboxes.tbl"),
            (Table::Emails, "emails.tbl"),
            (Table::Memberships, "memberships.tbl"),
            (Table::Keywords, "keywords.tbl"),
            (Table::Threads, "threads.tbl"),
            (Table::ThreadAnchors, "thread-anchors.tbl"),
            (Table::Submissions, "submissions.tbl"),
            (Table::Recipients, "recipients.tbl"),
            (Table::Leases, "leases.tbl"),
            (Table::Imports, "imports.tbl"),
        ];
        let mut destinations: Vec<_> = tables
            .into_iter()
            .map(|(table, name)| {
                (
                    MetadataDestination::Table(generation, table),
                    format!("{checkpoint}/{name}"),
                )
            })
            .collect();
        destinations.extend([
            (MetadataDestination::Manifest(generation), format!("{checkpoint}/manifest")),
            (MetadataDestination::Journal(generation), "accounts/03030303030303030303030303030303/metadata/journal/18446744073709551615.log".to_owned()),
        ]);
        for (index, (destination, expected)) in destinations.into_iter().enumerate() {
            let number = index as u64 + 1;
            let source = path(
                &fixture,
                AccountEntry::TemporaryFile(Number::new(number).unwrap()),
            );
            let target = fixture.path.join(&expected);
            let file = temporary(&root, number)
                .publish_metadata(destination)
                .unwrap();
            assert_eq!(file.name().as_str().unwrap(), expected);
            assert!(!source.exists());
            assert_eq!(fs::read(&target).unwrap(), b"hello");
            assert_eq!(fs::metadata(&target).unwrap().nlink(), 1);
            drop(file);
            fs::write(&target, b"prior").unwrap();
            assert!(
                matches!(temporary(&root, number).publish_metadata(destination), Err(PublishError::Rejected(e)) if e.kind() == io::ErrorKind::AlreadyExists)
            );
            assert_eq!(fs::read(&target).unwrap(), b"prior");
            assert_eq!(fs::read(&source).unwrap(), b"hello");
        }
        assert!(!path(&fixture, AccountEntry::Current).exists());
        // Missing generations cannot be silently created or selected.
        assert!(
            matches!(temporary(&root, 20).publish_metadata(MetadataDestination::Manifest(Number::new(1).unwrap())), Err(PublishError::Rejected(e)) if e.kind() == io::ErrorKind::NotFound)
        );
        assert!(!path(&fixture, AccountEntry::Checkpoint(Number::new(1).unwrap())).exists());
        assert!(!path(&fixture, AccountEntry::Manifest(Number::new(1).unwrap())).exists());
        assert_eq!(
            fs::read(path(
                &fixture,
                AccountEntry::TemporaryFile(Number::new(20).unwrap())
            ))
            .unwrap(),
            b"hello"
        );
    }

    #[test]
    fn publication_refuses_invalid_sources_and_destination_links() {
        for case in 0..7 {
            let fixture = Fixture::new();
            let root = fixture.locked();
            setup(&root);
            let file = temporary(&root, 1);
            let source = path(
                &fixture,
                AccountEntry::TemporaryFile(Number::new(1).unwrap()),
            );
            let target = path(&fixture, AccountEntry::Blob(BlobKind::Message, BLOB));
            match case {
                0 => fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap(),
                1 => fs::hard_link(&source, fixture.path.join("extra")).unwrap(),
                2 => {
                    fs::remove_file(&source).unwrap();
                    fs::write(&source, b"other").unwrap();
                    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
                }
                3 => fs::write(&source, b"longer").unwrap(),
                4 => {
                    fs::remove_file(&source).unwrap();
                    symlink("missing", &source).unwrap();
                }
                5 => symlink("missing", &target).unwrap(),
                _ => fs::remove_dir(target.parent().unwrap()).unwrap(),
            }
            assert!(matches!(
                file.publish_blob(BlobKind::Message, BLOB),
                Err(PublishError::Rejected(_))
            ));
            if case == 5 {
                assert!(fs::symlink_metadata(&target).unwrap().is_symlink());
            } else {
                assert!(!target.exists());
            }
            assert!(fs::symlink_metadata(&source).is_ok());
        }
    }

    #[test]
    fn publication_refuses_a_replaced_temporary_parent() {
        let fixture = Fixture::new();
        let root = fixture.locked();
        setup(&root);
        let file = temporary(&root, 1);
        let source = path(
            &fixture,
            AccountEntry::TemporaryFile(Number::new(1).unwrap()),
        );
        let target = path(&fixture, AccountEntry::Blob(BlobKind::Message, BLOB));
        let parent = source.parent().unwrap();
        let saved = fixture.path.join("saved-tmp");
        fs::rename(parent, &saved).unwrap();
        fs::create_dir(parent).unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::rename(saved.join(source.file_name().unwrap()), &source).unwrap();
        assert!(
            matches!(file.publish_blob(BlobKind::Message, BLOB), Err(PublishError::Rejected(e)) if e.kind() == io::ErrorKind::InvalidData)
        );
        assert_eq!(fs::read(&source).unwrap(), b"hello");
        assert!(!target.exists());
        assert_eq!(fs::read_dir(saved).unwrap().count(), 0);
    }

    pub(super) struct Fault {
        pub(super) point: usize,
        pub(super) step: usize,
        pub(super) target: fs::Metadata,
        pub(super) source: fs::Metadata,
    }
    impl Fault {
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
    impl Operations for Fault {
        fn link(&mut self, source: &Path, target: &Path) -> io::Result<()> {
            assert_eq!(self.step, 0);
            self.perform(|| fs::hard_link(source, target))
        }
        fn sync(&mut self, file: &File) -> io::Result<()> {
            assert!(self.step == 2 || self.step == 6);
            let expected = if self.step == 2 {
                &self.target
            } else {
                &self.source
            };
            let actual = file.metadata()?;
            assert_eq!(
                (actual.dev(), actual.ino()),
                (expected.dev(), expected.ino())
            );
            self.perform(|| file.sync_all())
        }
        fn unlink(&mut self, path: &Path) -> io::Result<()> {
            assert_eq!(self.step, 4);
            self.perform(|| fs::remove_file(path))
        }
    }

    #[test]
    fn publication_errors_retain_exact_last_established_boundary() {
        for point in 0..8 {
            let fixture = Fixture::new();
            let root = fixture.locked();
            setup(&root);
            let file = temporary(&root, 1);
            let source = path(
                &fixture,
                AccountEntry::TemporaryFile(Number::new(1).unwrap()),
            );
            let target = path(&fixture, AccountEntry::Blob(BlobKind::Message, BLOB));
            let fault = Fault {
                point,
                step: 0,
                target: fs::metadata(target.parent().unwrap()).unwrap(),
                source: fs::metadata(source.parent().unwrap()).unwrap(),
            };
            let error = file
                .publish_using(BlobKind::Message, BLOB, fault)
                .unwrap_err();
            let cause = match (point, error) {
                (0..=1, PublishError::LinkAttempted(e))
                | (2..=3, PublishError::Linked(e))
                | (4..=5, PublishError::DestinationSynced(e))
                | (6..=7, PublishError::TemporaryUnlinked(e)) => e,
                other => panic!("wrong boundary: {other:?}"),
            };
            assert_eq!(cause.kind(), io::ErrorKind::StorageFull);
            assert_eq!(source.exists(), point < 5);
            assert_eq!(target.exists(), point >= 1);
            if source.exists() {
                assert_eq!(fs::read(&source).unwrap(), b"hello");
            }
            if target.exists() {
                assert_eq!(fs::read(&target).unwrap(), b"hello");
            }
        }
    }
}
