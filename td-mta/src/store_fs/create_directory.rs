//! Exclusive private-directory creation under the held writer lock.
use super::{CreateError, Directory, LockedRoot, MAX_PATH_BYTES};
use crate::{
    ids::AccountId,
    store_paths::{AccountEntry, Name, RootEntry},
};
use std::{
    fs::{self, File},
    io,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::Path,
};

impl LockedRoot {
    /// Creates and syncs only the top-level accounts directory. The caller
    /// charges it before this operation; a collision is never adopted.
    pub fn create_accounts_directory(&self) -> Result<Directory, CreateError> {
        let name = Name::root(RootEntry::Accounts)
            .map_err(|_| CreateError::Uncreated(io::ErrorKind::InvalidInput.into()))?;
        self.create_directory(&name)
    }
    /// Creates one typed directory; parents must already be private and durable.
    /// No account authorization, logical charge or generation selection follows.
    pub fn create_account_directory(
        &self,
        account: AccountId,
        entry: AccountEntry,
    ) -> Result<Directory, CreateError> {
        if !matches!(
            entry,
            AccountEntry::Root
                | AccountEntry::Messages
                | AccountEntry::Uploads
                | AccountEntry::Metadata
                | AccountEntry::Checkpoints
                | AccountEntry::Checkpoint(_)
                | AccountEntry::Journals
                | AccountEntry::Cache
                | AccountEntry::Temporary
                | AccountEntry::Shard(_, _)
        ) {
            return Err(CreateError::Uncreated(io::ErrorKind::InvalidInput.into()));
        }
        let name = Name::account(account, entry)
            .map_err(|_| CreateError::Uncreated(io::ErrorKind::InvalidInput.into()))?;
        self.create_directory(&name)
    }
    fn create_directory(&self, name: &Name) -> Result<Directory, CreateError> {
        create_using(&self.root.directory, name, create, File::sync_all)
    }
}

fn create(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(false)
        .mode(0o700)
        .create(path)
}
fn create_using(
    root: &Directory,
    name: &Name,
    create: impl FnOnce(&Path) -> io::Result<()>,
    mut sync: impl FnMut(&File) -> io::Result<()>,
) -> Result<Directory, CreateError> {
    let mut path = [0; MAX_PATH_BYTES];
    let destination = root
        .destination(name, &mut path)
        .map_err(CreateError::Uncreated)?;
    super::require_absent(destination.path).map_err(CreateError::Uncreated)?;
    create(destination.path).map_err(CreateError::Attempted)?;
    // The trusted stable namespace permits restoring umask-filtered owner bits
    // before opening a new directory whose initial mode may prohibit access.
    fs::set_permissions(destination.path, fs::Permissions::from_mode(0o700))
        .map_err(CreateError::Created)?;
    super::private_directory(destination.path, destination.owner).map_err(CreateError::Created)?;
    let directory = Directory::from_path(
        destination
            .path
            .to_str()
            .ok_or_else(|| CreateError::Created(io::ErrorKind::InvalidInput.into()))?,
    )
    .map_err(CreateError::Created)?;
    sync(&directory.file).map_err(CreateError::Created)?;
    sync(&destination.parent.file).map_err(CreateError::Created)?;
    Ok(directory)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;
    use crate::{format::row::BlobKind, ids::BlobId, store_paths::Number};
    use std::os::unix::fs::{symlink, MetadataExt};

    #[test]
    fn private_hierarchy_is_exclusive_and_rejects_file_entries() {
        let fixture = Fixture::new();
        let root = fixture.locked();
        let account = AccountId::from_bytes([8; 16]);
        let number = Number::new(1).unwrap();
        assert!(
            matches!(root.create_account_directory(account, AccountEntry::Root), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::NotFound)
        );
        root.create_accounts_directory().unwrap();
        assert!(
            matches!(root.create_accounts_directory(), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::AlreadyExists)
        );
        for entry in [
            AccountEntry::Root,
            AccountEntry::Messages,
            AccountEntry::Uploads,
            AccountEntry::Metadata,
            AccountEntry::Checkpoints,
            AccountEntry::Checkpoint(number),
            AccountEntry::Journals,
            AccountEntry::Cache,
            AccountEntry::Temporary,
            AccountEntry::Shard(BlobKind::Message, 1),
            AccountEntry::Shard(BlobKind::Upload, 2),
        ] {
            let directory = root.create_account_directory(account, entry).unwrap();
            let metadata = directory.metadata().unwrap();
            assert_eq!(metadata.mode() & 0o7777, 0o700);
            assert_eq!(metadata.uid(), fs::metadata(&fixture.path).unwrap().uid());
            let path = fixture
                .path
                .join(Name::account(account, entry).unwrap().as_path().unwrap());
            fs::write(path.join("marker"), b"preserve").unwrap();
            assert!(
                matches!(root.create_account_directory(account, entry), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::AlreadyExists)
            );
            assert_eq!(fs::read(path.join("marker")).unwrap(), b"preserve");
        }
        for entry in [
            AccountEntry::Current,
            AccountEntry::Manifest(number),
            AccountEntry::Journal(number),
            AccountEntry::TemporaryFile(number),
            AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([1; 16])),
            AccountEntry::Table(number, crate::format::Table::Emails),
        ] {
            assert!(
                matches!(root.create_account_directory(account, entry), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::InvalidInput)
            );
        }
    }

    #[test]
    fn directory_collisions_never_modify_files_links_or_modes() {
        let fixture = Fixture::new();
        let root = fixture.locked();
        let path = fixture.path.join("accounts");
        fs::write(&path, b"preserve").unwrap();
        assert!(
            matches!(root.create_accounts_directory(), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::AlreadyExists)
        );
        assert_eq!(fs::read(&path).unwrap(), b"preserve");
        fs::remove_file(&path).unwrap();
        symlink("missing", &path).unwrap();
        assert!(
            matches!(root.create_accounts_directory(), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::AlreadyExists)
        );
        assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
        fs::remove_file(&path).unwrap();
        fs::DirBuilder::new().mode(0o500).create(&path).unwrap();
        assert!(
            matches!(root.create_accounts_directory(), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::AlreadyExists)
        );
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o500);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(
            matches!(root.create_account_directory(AccountId::from_bytes([2;16]), AccountEntry::Root), Err(CreateError::Uncreated(e)) if e.kind() == io::ErrorKind::PermissionDenied)
        );
        fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn directory_owner_bits_are_restored_before_open() {
        let fixture = Fixture::new();
        let root = fixture.locked();
        let name = Name::root(RootEntry::Accounts).unwrap();
        let directory = create_using(
            &root.root.directory,
            &name,
            |path| fs::DirBuilder::new().mode(0o000).create(path),
            File::sync_all,
        )
        .unwrap();
        assert_eq!(directory.metadata().unwrap().mode() & 0o7777, 0o700);
    }

    #[test]
    fn directory_creation_and_sync_errors_preserve_possible_effects() {
        for point in 0..6 {
            let fixture = Fixture::new();
            let root = fixture.locked();
            let name = Name::root(RootEntry::Accounts).unwrap();
            let mut sync_step = 2;
            let result = create_using(
                &root.root.directory,
                &name,
                |path| {
                    if point == 0 {
                        return Err(io::ErrorKind::StorageFull.into());
                    }
                    create(path)?;
                    if point == 1 {
                        return Err(io::ErrorKind::StorageFull.into());
                    }
                    Ok(())
                },
                |file| {
                    let expected = if sync_step == 2 {
                        fs::metadata(fixture.path.join("accounts")).unwrap()
                    } else {
                        fs::metadata(&fixture.path).unwrap()
                    };
                    let actual = file.metadata().unwrap();
                    assert_eq!(
                        (actual.dev(), actual.ino()),
                        (expected.dev(), expected.ino())
                    );
                    if point == sync_step {
                        return Err(io::ErrorKind::StorageFull.into());
                    }
                    sync_step += 1;
                    file.sync_all()?;
                    if point == sync_step {
                        return Err(io::ErrorKind::StorageFull.into());
                    }
                    sync_step += 1;
                    Ok(())
                },
            );
            if point < 2 {
                assert!(
                    matches!(result, Err(CreateError::Attempted(e)) if e.kind() == io::ErrorKind::StorageFull)
                );
            } else {
                assert!(
                    matches!(result, Err(CreateError::Created(e)) if e.kind() == io::ErrorKind::StorageFull)
                );
            }
            assert_eq!(fixture.path.join("accounts").exists(), point != 0);
        }
    }
}
