//! Captured journal bytes; later appends do not extend this reader's authority.
use super::{super::LockedRoot, open_extent_using, Extent, StoreReader};
use crate::{
    ids::AccountId,
    store_paths::{AccountEntry, Name, Number},
};
use std::{fs::File, io};

impl LockedRoot {
    /// Caller authorizes the account and holds the real committed-prefix pin.
    /// The physical ceiling applies at open; later suffix bytes are not read.
    pub fn open_journal_prefix(
        &self,
        account: AccountId,
        segment: Number,
        prefix_bytes: u64,
        physical_max_bytes: u64,
    ) -> io::Result<PrefixReader<'_>> {
        if prefix_bytes > physical_max_bytes || physical_max_bytes > i64::MAX as u64 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let name = Name::account(account, AccountEntry::Journal(segment))
            .map_err(|_| io::ErrorKind::InvalidInput)?;
        let (file, length) = open_extent_using(
            &self.root.directory,
            &name,
            physical_max_bytes,
            Extent::Prefix(prefix_bytes),
            |path| File::open(path),
        )?;
        Ok(PrefixReader {
            input: StoreReader {
                owner: self,
                file,
                name,
                length,
                position: 0,
                failed: false,
            },
        })
    }
}

#[derive(Debug)]
pub struct PrefixReader<'a> {
    input: StoreReader<'a>,
}
impl<'a> PrefixReader<'a> {
    pub fn name(&self) -> &Name {
        self.input.name()
    }
    pub fn len(&self) -> u64 {
        self.input.len()
    }
    pub fn is_empty(&self) -> bool {
        self.input.is_empty()
    }
    pub fn position(&self) -> u64 {
        self.input.position()
    }
    pub fn is_failed(&self) -> bool {
        self.input.is_failed()
    }
    /// At most one bounded read; zero is not physical EOF evidence.
    pub fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.input.read(output)
    }
    /// Consumed prefix only; no EOF read and no whole-file completion conversion.
    pub fn finish(self) -> io::Result<CompletePrefix<'a>> {
        let input = self.input;
        if input.failed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if input.position != input.length {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if input.file.metadata()?.len() < input.length {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(CompletePrefix {
            owner: input.owner,
            file: input.file,
            name: input.name,
            length: input.length,
        })
    }
}

/// Proves consumed bytes only, not format validity, physical EOF or pin ownership.
#[derive(Debug)]
pub struct CompletePrefix<'a> {
    owner: &'a LockedRoot,
    file: File,
    name: Name,
    length: u64,
}
impl CompletePrefix<'_> {
    pub(crate) fn belongs_to(&self, root: &LockedRoot) -> bool {
        std::ptr::eq(self.owner, root)
    }
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
        super::super::temporary::read_extent(&self.file, self.length, offset, output)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) fn probe(root: &LockedRoot) {
    // Reads the literal journal installed by history::prepare_probe before measurement.
    let account = AccountId::from_bytes([0x33; 16]);
    let segment = Number::new(1).unwrap();
    let mut input = root.open_journal_prefix(account, segment, 96, 277).unwrap();
    let mut output = [0; 128];
    assert_eq!(input.read(&mut output).unwrap(), 96);
    assert_eq!(input.read(&mut output).unwrap(), 0);
    let complete = input.finish().unwrap();
    assert_eq!(complete.len(), 96);
    assert_eq!(complete.read_at(95, &mut output).unwrap(), 1);
    assert_eq!(complete.read_at(96, &mut output).unwrap(), 0);
    assert_eq!(
        complete.read_at(97, &mut output).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        root.open_journal_prefix(account, segment, 278, 300)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(
        root.open_journal_prefix(account, segment, 96, 276)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(
        root.open_journal_prefix(account, segment, 97, 96)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        root.open_journal_prefix(account, segment, 96, 277)
            .unwrap()
            .finish()
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    let mut input = root.open_journal_prefix(account, segment, 96, 277).unwrap();
    assert_eq!(
        input
            .input
            .read_using(
                &mut output,
                |_, _, _| Err(io::ErrorKind::Interrupted.into())
            )
            .unwrap_err()
            .kind(),
        io::ErrorKind::Interrupted
    );
    assert!(input.is_failed());
    assert_eq!(
        input.read(&mut output).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(
        input.finish().unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::super::{tests::Fixture, MAX_FILE_STEP_BYTES};
    use super::*;
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        os::unix::fs::{symlink, PermissionsExt},
        path::PathBuf,
    };
    const ACCOUNT: AccountId = AccountId::from_bytes([7; 16]);
    fn segment() -> Number {
        Number::new(1).unwrap()
    }
    fn setup(fixture: &Fixture, bytes: &[u8]) -> (LockedRoot, PathBuf) {
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Metadata,
            AccountEntry::Journals,
        ] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
        let name = Name::account(ACCOUNT, AccountEntry::Journal(segment())).unwrap();
        let path = fixture.path.join(name.as_path().unwrap());
        write(&path, bytes);
        (root, path)
    }
    fn write(path: &std::path::Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[test]
    fn captured_prefix_excludes_existing_and_later_suffixes() {
        let fixture = Fixture::new();
        let (root, path) = setup(&fixture, b"hello suffix");
        let mut input = root.open_journal_prefix(ACCOUNT, segment(), 5, 12).unwrap();
        assert_eq!(input.len(), 5);
        assert_eq!(input.position(), 0);
        assert!(!input.is_empty());
        assert_eq!(
            input.name(),
            &Name::account(ACCOUNT, AccountEntry::Journal(segment())).unwrap()
        );
        let mut output = [0xa5; 20];
        assert_eq!(input.read(&mut []).unwrap(), 0);
        assert_eq!(input.read(&mut output).unwrap(), 5);
        assert_eq!(output.get(..5).unwrap(), b"hello");
        assert!(output.get(5..).unwrap().iter().all(|b| *b == 0xa5));
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b" later")
            .unwrap();
        assert_eq!(input.read(&mut output).unwrap(), 0);
        let complete = input.finish().unwrap();
        assert_eq!(complete.len(), 5);
        assert!(!complete.is_empty());
        assert_eq!(complete.read_at(3, &mut output).unwrap(), 2);
        assert_eq!(output.get(..2).unwrap(), b"lo");
        assert_eq!(complete.read_at(5, &mut output).unwrap(), 0);
        assert_eq!(
            complete.read_at(6, &mut output).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            complete.name(),
            &Name::account(ACCOUNT, AccountEntry::Journal(segment())).unwrap()
        );
        let empty = root.open_journal_prefix(ACCOUNT, segment(), 0, 20).unwrap();
        assert!(empty.is_empty());
        assert!(empty.finish().unwrap().is_empty());
    }
    #[test]
    fn prefix_requires_admitted_present_private_journal() {
        let fixture = Fixture::new();
        let (root, path) = setup(&fixture, b"hello");
        for (prefix, cap, kind) in [
            (6, 5, io::ErrorKind::InvalidInput),
            (0, u64::MAX, io::ErrorKind::InvalidInput),
            (6, 6, io::ErrorKind::InvalidData),
            (4, 4, io::ErrorKind::InvalidData),
        ] {
            assert_eq!(
                root.open_journal_prefix(ACCOUNT, segment(), prefix, cap)
                    .unwrap_err()
                    .kind(),
                kind
            );
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            root.open_journal_prefix(ACCOUNT, segment(), 5, 5)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let extra = fixture.path.join("extra");
        fs::hard_link(&path, &extra).unwrap();
        assert_eq!(
            root.open_journal_prefix(ACCOUNT, segment(), 5, 5)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_file(&path).unwrap();
        symlink(&extra, &path).unwrap();
        assert_eq!(
            root.open_journal_prefix(ACCOUNT, segment(), 5, 5)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn prefix_open_allows_only_same_inode_monotonic_growth() {
        let fixture = Fixture::new();
        let (root, path) = setup(&fixture, b"hello");
        let name = Name::account(ACCOUNT, AccountEntry::Journal(segment())).unwrap();
        for (extent, changed, succeeds) in [
            (Extent::Prefix(3), 8, true),
            (Extent::Whole, 8, false),
            (Extent::Prefix(3), 4, false),
            (Extent::Prefix(3), 9, false),
            (Extent::Whole, 5, true),
        ] {
            write(&path, b"hello");
            let result = open_extent_using(&root.root.directory, &name, 8, extent, |path| {
                OpenOptions::new()
                    .write(true)
                    .open(path)?
                    .set_len(changed)?;
                File::open(path)
            });
            if succeeds {
                assert!(result.is_ok());
            } else {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
            }
        }
        write(&path, b"hello");
        let replacement = fixture.path.join("replacement");
        write(&replacement, b"replacement");
        assert_eq!(
            open_extent_using(&root.root.directory, &name, 20, Extent::Prefix(3), |path| {
                fs::rename(&replacement, path)?;
                File::open(path)
            })
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn prefix_reads_are_bounded_and_failures_cannot_complete() {
        let fixture = Fixture::new();
        let bytes = vec![1; MAX_FILE_STEP_BYTES + 2];
        let (root, path) = setup(&fixture, &bytes);
        let mut input = root
            .open_journal_prefix(ACCOUNT, segment(), bytes.len() as u64, bytes.len() as u64)
            .unwrap();
        let mut output = vec![0; bytes.len()];
        assert_eq!(input.read(&mut output).unwrap(), MAX_FILE_STEP_BYTES);
        assert_eq!(input.read(&mut output).unwrap(), 2);
        input.finish().unwrap();
        for consumed in [false, true] {
            write(&path, b"hello");
            let mut input = root.open_journal_prefix(ACCOUNT, segment(), 5, 5).unwrap();
            if consumed {
                assert_eq!(input.read(&mut output).unwrap(), 5);
            }
            OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(3)
                .unwrap();
            if consumed {
                assert_eq!(
                    input.finish().unwrap_err().kind(),
                    io::ErrorKind::InvalidData
                );
            } else {
                assert_eq!(input.read(&mut output).unwrap(), 3);
                assert_eq!(
                    input.read(&mut output).unwrap_err().kind(),
                    io::ErrorKind::UnexpectedEof
                );
                assert!(input.is_failed());
                assert_eq!(
                    input.read(&mut output).unwrap_err().kind(),
                    io::ErrorKind::BrokenPipe
                );
                assert_eq!(
                    input.finish().unwrap_err().kind(),
                    io::ErrorKind::BrokenPipe
                );
            }
        }
        write(&path, b"hello");
        assert_eq!(
            root.open_journal_prefix(ACCOUNT, segment(), 5, 5)
                .unwrap()
                .finish()
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
