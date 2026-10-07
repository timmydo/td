//! Bounded reads of typed private store files; parsing and view pins are external.
use super::{Directory, LockedRoot, MAX_PATH_BYTES};
use crate::{
    ids::AccountId,
    store_paths::{AccountEntry, Name},
};
use std::{
    fs::{self, File},
    io,
    os::unix::fs::{FileExt, MetadataExt},
};

impl LockedRoot {
    /// Caller authorizes the account, retains the selected live view/extent and
    /// admits the read work. No directory, LOCK or temporary role is accepted.
    pub fn open_account_file(
        &self,
        account: AccountId,
        entry: AccountEntry,
        max_bytes: u64,
    ) -> io::Result<StoreReader<'_>> {
        if !matches!(entry, AccountEntry::Blob(_, _)) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let name = Name::account(account, entry).map_err(|_| io::ErrorKind::InvalidInput)?;
        self.open_input(name, max_bytes)
    }
    fn open_input(&self, name: Name, max_bytes: u64) -> io::Result<StoreReader<'_>> {
        if max_bytes > i64::MAX as u64 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let (file, length) = open(&self.root.directory, &name, max_bytes)?;
        Ok(StoreReader {
            _owner: self,
            file,
            name,
            length,
            position: 0,
            failed: false,
        })
    }
}

#[derive(Debug)]
pub struct StoreReader<'a> {
    _owner: &'a LockedRoot,
    file: File,
    name: Name,
    length: u64,
    position: u64,
    failed: bool,
}
impl<'a> StoreReader<'a> {
    pub fn name(&self) -> &Name {
        &self.name
    }
    pub fn len(&self) -> u64 {
        self.length
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub fn position(&self) -> u64 {
        self.position
    }
    pub fn is_failed(&self) -> bool {
        self.failed
    }
    /// One explicit read at most, bounded by caller buffer, remaining extent and
    /// MAX_FILE_STEP_BYTES. Zero (including an empty buffer) is not EOF evidence.
    pub fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.read_using(output, File::read_at)
    }
    fn read_using(
        &mut self,
        output: &mut [u8],
        read: impl FnOnce(&File, &mut [u8], u64) -> io::Result<usize>,
    ) -> io::Result<usize> {
        if self.failed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let remaining = self
            .length
            .checked_sub(self.position)
            .ok_or(io::ErrorKind::InvalidData)?;
        let count = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(output.len())
            .min(super::MAX_FILE_STEP_BYTES);
        if count == 0 {
            return Ok(0);
        }
        self.failed = true;
        let bytes = output.get_mut(..count).ok_or(io::ErrorKind::InvalidInput)?;
        let actual = read(&self.file, bytes, self.position)?;
        if actual == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if actual > count {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.position = self
            .position
            .checked_add(actual as u64)
            .ok_or(io::ErrorKind::InvalidData)?;
        self.failed = false;
        Ok(actual)
    }
    /// Consumes only a fully read, unchanged extent with observed physical EOF.
    /// The caller separately verifies parsed summaries/digests and view pins.
    pub fn finish(self) -> io::Result<CompleteFile<'a>> {
        self.finish_using(File::read_at)
    }
    fn finish_using(
        self,
        read: impl FnOnce(&File, &mut [u8], u64) -> io::Result<usize>,
    ) -> io::Result<CompleteFile<'a>> {
        if self.failed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if self.position != self.length {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if self.file.metadata()?.len() != self.length
            || read(&self.file, &mut [0; 1], self.length)? != 0
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(CompleteFile {
            _owner: self._owner,
            file: self.file,
            name: self.name,
            length: self.length,
        })
    }
}

/// Descriptor extent was consumed and EOF observed. This proves no file format,
/// digest, current pathname binding, read-view pin or authorization.
#[derive(Debug)]
pub struct CompleteFile<'a> {
    _owner: &'a LockedRoot,
    file: File,
    name: Name,
    length: u64,
}
impl CompleteFile<'_> {
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
        super::temporary::read_extent(&self.file, self.length, offset, output)
    }
}
fn open(root: &Directory, name: &Name, max_bytes: u64) -> io::Result<(File, u64)> {
    open_extent_using(root, name, max_bytes, |path| File::open(path))
}
fn open_extent_using(
    root: &Directory,
    name: &Name,
    max_bytes: u64,
    open_file: impl FnOnce(&std::path::Path) -> io::Result<File>,
) -> io::Result<(File, u64)> {
    let mut buffer = [0; MAX_PATH_BYTES];
    let source = root.destination(name, &mut buffer)?;
    let before = fs::symlink_metadata(source.path)?;
    check(&before, source.owner, max_bytes)?;
    let file = open_file(source.path)?;
    let after = file.metadata()?;
    check(&after, source.owner, max_bytes)?;
    if before.len() != after.len() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let length = after.len();
    if !super::same_file(&before, &after) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok((file, length))
}
fn check(metadata: &fs::Metadata, owner: u32, max_bytes: u64) -> io::Result<()> {
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() > max_bytes
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;
    use crate::{format::row::BlobKind, ids::BlobId, store_paths::Number};
    use std::{
        io::Write,
        os::unix::fs::{symlink, PermissionsExt},
        path::{Path, PathBuf},
    };
    const ACCOUNT: AccountId = AccountId::from_bytes([5; 16]);
    fn setup(root: &LockedRoot) {
        root.create_accounts_directory().unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Messages,
            AccountEntry::Shard(BlobKind::Message, 0x09),
        ] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
    }
    fn path(fixture: &Fixture, entry: AccountEntry) -> PathBuf {
        fixture
            .path
            .join(Name::account(ACCOUNT, entry).unwrap().as_path().unwrap())
    }
    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[test]
    fn typed_store_reads_need_complete_extent_and_eof() {
        let fixture = Fixture::new();
        let root = fixture.locked();
        setup(&root);
        {
            let entry = AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16]));
            write(&path(&fixture, entry), b"hello");
            let mut file = root.open_account_file(ACCOUNT, entry, 5).unwrap();
            assert_eq!(file.name(), &Name::account(ACCOUNT, entry).unwrap());
            assert_eq!(file.len(), 5);
            assert!(!file.is_empty());
            assert_eq!(file.position(), 0);
            assert_eq!(file.read(&mut []).unwrap(), 0);
            let mut bytes = [0; 8];
            assert_eq!(
                file.read_using(&mut bytes, |file, output, offset| file
                    .read_at(output.get_mut(..2).unwrap(), offset))
                    .unwrap(),
                2
            );
            assert_eq!(file.position(), 2);
            assert_eq!(file.read(&mut bytes).unwrap(), 3);
            assert_eq!(bytes.get(..3), Some(b"llo".as_slice()));
            assert_eq!(file.read(&mut bytes).unwrap(), 0);
            let complete = file.finish().unwrap();
            assert_eq!(complete.name(), &Name::account(ACCOUNT, entry).unwrap());
            assert_eq!(complete.len(), 5);
            assert!(!complete.is_empty());
            assert_eq!(complete.read_at(0, &mut bytes).unwrap(), 5);
            assert_eq!(bytes.get(..5), Some(b"hello".as_slice()));
            assert_eq!(
                root.open_account_file(ACCOUNT, entry, 5)
                    .unwrap()
                    .finish()
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        write(
            &path(
                &fixture,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
            ),
            b"",
        );
        let file = root
            .open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                0,
            )
            .unwrap();
        assert!(file.is_empty());
        assert!(file.finish().unwrap().is_empty());
    }
    #[test]
    fn selected_inputs_refuse_nonprivate_links_sizes_and_unselected_roles() {
        let fixture = Fixture::new();
        let root = fixture.locked();
        setup(&root);
        let number = Number::new(1).unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Temporary,
            AccountEntry::TemporaryFile(number),
        ] {
            assert_eq!(
                root.open_account_file(ACCOUNT, entry, 5)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        let current = path(
            &fixture,
            AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
        );
        write(&current, b"hello");
        assert_eq!(
            root.open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                4
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            root.open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                u64::MAX
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidInput
        );
        fs::set_permissions(&current, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(root
            .open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                5
            )
            .is_err());
        fs::set_permissions(&current, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&current, fixture.path.join("extra")).unwrap();
        assert!(root
            .open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                5
            )
            .is_err());
        fs::remove_file(&current).unwrap();
        symlink(fixture.path.join("extra"), &current).unwrap();
        assert!(root
            .open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                5
            )
            .is_err());
        fs::remove_file(&current).unwrap();
        fs::create_dir(&current).unwrap();
        assert!(root
            .open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                5
            )
            .is_err());
        fs::remove_dir(&current).unwrap();
        let socket = fixture.path.join("socket");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        fs::rename(&socket, &current).unwrap();
        fs::set_permissions(&current, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            root.open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                5
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
        drop(listener);
        fs::remove_file(&current).unwrap();
        assert_eq!(
            root.open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                5
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::NotFound
        );
        write(&current, b"hello");
        let parent = current.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o750)).unwrap();
        assert_eq!(
            root.open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                5
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[test]
    fn reader_errors_and_changed_physical_extents_cannot_complete() {
        let fixture = Fixture::new();
        let root = fixture.locked();
        setup(&root);
        let current = path(
            &fixture,
            AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
        );
        for error in [io::ErrorKind::Interrupted, io::ErrorKind::PermissionDenied] {
            write(&current, b"hello");
            let mut file = root
                .open_account_file(
                    ACCOUNT,
                    AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                    5,
                )
                .unwrap();
            assert_eq!(
                file.read_using(&mut [0; 5], |_, _, _| Err(error.into()))
                    .unwrap_err()
                    .kind(),
                error
            );
            assert!(file.is_failed());
            assert_eq!(file.position(), 0);
            assert_eq!(
                file.read(&mut [0; 5]).unwrap_err().kind(),
                io::ErrorKind::BrokenPipe
            );
            assert_eq!(file.finish().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        }
        write(&current, b"hello");
        let mut file = root
            .open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                5,
            )
            .unwrap();
        assert_eq!(
            file.read_using(&mut [0; 5], |_, bytes, _| Ok(bytes.len() + 1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(file.is_failed());
        assert_eq!(file.position(), 0);
        assert_eq!(
            file.read(&mut [0; 5]).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(file.finish().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        let mut file = root
            .open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                5,
            )
            .unwrap();
        assert_eq!(file.read(&mut [0; 5]).unwrap(), 5);
        write(&current, b"");
        assert_eq!(
            file.finish().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        write(&current, b"hello");
        let mut file = root
            .open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                5,
            )
            .unwrap();
        write(&current, b"");
        assert_eq!(
            file.read(&mut [0; 5]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(file.finish().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        write(&current, b"hello");
        let mut file = root
            .open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                5,
            )
            .unwrap();
        file.read(&mut [0; 5]).unwrap();
        write(&current, b"longer");
        assert_eq!(
            file.finish().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        write(&current, b"hello");
        let mut file = root
            .open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                5,
            )
            .unwrap();
        file.read(&mut [0; 5]).unwrap();
        assert_eq!(
            file.finish_using(|file, bytes, offset| {
                fs::OpenOptions::new()
                    .append(true)
                    .open(&current)?
                    .write_all(b"!")?;
                file.read_at(bytes, offset)
            })
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn large_reads_return_to_worker_at_the_step_bound() {
        let fixture = Fixture::new();
        let root = fixture.locked();
        setup(&root);
        let count = super::super::MAX_FILE_STEP_BYTES;
        write(
            &path(
                &fixture,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
            ),
            &vec![0x5a; count + 1],
        );
        let mut file = root
            .open_account_file(
                ACCOUNT,
                AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([9; 16])),
                (count + 1) as u64,
            )
            .unwrap();
        let mut bytes = vec![0; count + 1];
        assert_eq!(file.read(&mut bytes).unwrap(), count);
        assert_eq!(bytes.last(), Some(&0));
        assert_eq!(file.read(&mut bytes).unwrap(), 1);
        assert_eq!(file.finish().unwrap().len(), (count + 1) as u64);
    }
}
