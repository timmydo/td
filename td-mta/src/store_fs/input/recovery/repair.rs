//! Explicit incomplete-tail repair of a stopped journal; no arbitrary truncate offset.
#[cfg(test)]
use super::super::super::CurrentUpdate;
use super::super::super::LockedRoot;
use super::super::{fill_exact, open_extent_using, CompleteFile, Extent, StoreReader};
use super::{RecoveryInputError, ScannedJournal};
use crate::{
    format::{container::Current, journal_stream::Summary, CURRENT_BYTES},
    ports::Crypto,
    store_paths::AccountEntry,
};
use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::FileExt,
};

#[derive(Debug)]
pub enum RepairError {
    Rejected(RecoveryInputError),
    TruncateAttempted(io::Error),
    Truncated(io::Error),
    Synced(io::Error),
}
impl std::fmt::Display for RepairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Rejected(_) => "journal repair refused before truncation",
            Self::TruncateAttempted(_) => "journal truncation has uncertain effects",
            Self::Truncated(_) => "journal truncated but sync failed",
            Self::Synced(_) => "journal synced but repaired extent confirmation failed",
        })
    }
}
impl std::error::Error for RepairError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected(e) => Some(e),
            Self::TruncateAttempted(e) | Self::Truncated(e) | Self::Synced(e) => Some(e),
        }
    }
}
impl<'r> ScannedJournal<'r> {
    /// Requires actual stopped-store serialization and the still-selected CURRENT.
    /// No retry or rollback follows any attempted truncation error.
    pub fn repair(self, crypto: &impl Crypto) -> Result<RepairedJournal<'r>, RepairError> {
        self.repair_using(crypto, Real)
    }
    fn repair_using(
        self,
        crypto: &impl Crypto,
        mut ops: impl Operations,
    ) -> Result<RepairedJournal<'r>, RepairError> {
        if !self.has_incomplete_tail() {
            return Err(RepairError::Rejected(
                io::Error::from(io::ErrorKind::InvalidInput).into(),
            ));
        }
        let root = self.file.owner;
        check_current(root, crypto, self.current).map_err(RepairError::Rejected)?;
        let (writable, length) = open_extent_using(
            &root.root.directory,
            &self.file.name,
            self.file.length,
            Extent::Whole,
            |path| OpenOptions::new().read(true).write(true).open(path),
        )
        .map_err(|e| RepairError::Rejected(e.into()))?;
        let original = self
            .file
            .file
            .metadata()
            .map_err(|e| RepairError::Rejected(e.into()))?;
        let opened = writable
            .metadata()
            .map_err(|e| RepairError::Rejected(e.into()))?;
        if length != self.file.length
            || original.len() != self.file.length
            || opened.len() != self.file.length
            || !super::super::super::same_file(&original, &opened)
        {
            return Err(RepairError::Rejected(
                io::Error::from(io::ErrorKind::InvalidData).into(),
            ));
        }
        ops.truncate(&writable, self.prefix)
            .map_err(RepairError::TruncateAttempted)?;
        ops.sync(&writable).map_err(RepairError::Truncated)?;
        drop(writable);
        let input = StoreReader {
            owner: root,
            file: self.file.file,
            name: self.file.name,
            length: self.prefix,
            position: self.prefix,
            failed: false,
        };
        let file = input
            .finish_using(|file, output, offset| ops.read(file, output, offset))
            .map_err(RepairError::Synced)?;
        Ok(RepairedJournal {
            file,
            summary: self.summary,
        })
    }
}
fn check_current(
    root: &LockedRoot,
    crypto: &impl Crypto,
    expected: Current,
) -> Result<(), RecoveryInputError> {
    let mut encoded = [0; CURRENT_BYTES];
    expected.encode(crypto, &mut encoded)?;
    let mut input = root.open_account_file(
        expected.account,
        AccountEntry::Current,
        CURRENT_BYTES as u64,
    )?;
    if input.len() != CURRENT_BYTES as u64 {
        return Err(io::Error::from(io::ErrorKind::InvalidData).into());
    }
    let mut actual = [0; CURRENT_BYTES];
    let mut attempts = super::super::super::journal_input::MAX_READ_CALLS;
    fill_exact(&mut input, &mut actual, &mut attempts)?;
    input.finish()?;
    if actual != encoded {
        return Err(io::Error::from(io::ErrorKind::InvalidData).into());
    }
    Ok(())
}
#[derive(Debug)]
pub struct RepairedJournal<'r> {
    file: CompleteFile<'r>,
    summary: Summary,
}
impl RepairedJournal<'_> {
    pub fn file(&self) -> &CompleteFile<'_> {
        &self.file
    }
    pub fn summary(&self) -> Summary {
        self.summary
    }
}
trait Operations {
    fn truncate(&mut self, file: &File, length: u64) -> io::Result<()>;
    fn sync(&mut self, file: &File) -> io::Result<()>;
    fn read(&mut self, file: &File, output: &mut [u8], offset: u64) -> io::Result<usize>;
}
struct Real;
impl Operations for Real {
    fn truncate(&mut self, file: &File, length: u64) -> io::Result<()> {
        file.set_len(length)
    }
    fn sync(&mut self, file: &File) -> io::Result<()> {
        file.sync_all()
    }
    fn read(&mut self, file: &File, output: &mut [u8], offset: u64) -> io::Result<usize> {
        file.read_at(output, offset)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) fn prepare_probe() -> CurrentUpdate {
    use super::fixture::{bytes, hex};
    let previous = Current::decode(
        &td_crypto::Provider,
        &hex(include_str!(
            "../../../../tests/fixtures/format-v1/current.hex"
        )),
    )
    .unwrap();
    CurrentUpdate::prepare(
        &td_crypto::Provider,
        Some(previous),
        bytes().selection().current(),
    )
    .unwrap()
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) fn probe(
    root: &LockedRoot,
    bytes: &super::ProbeBytes,
    update: &CurrentUpdate,
    scratch: &mut [u8; crate::format::MAX_FRAME_BYTES],
) {
    use crate::store_paths::Number;
    use td_crypto::Provider;
    // Earlier allocation fixtures used CURRENT generation 1 and supplied generation 2 metadata.
    let mut stale = root
        .scan_active_journal(&Provider, bytes.selection(), 273, scratch)
        .unwrap();
    assert!(stale.next_frame().unwrap().is_some());
    assert!(stale.next_frame().unwrap().is_none());
    assert!(
        matches!(stale.finish().unwrap().repair(&Provider), Err(RepairError::Rejected(RecoveryInputError::Io(e))) if e.kind() == io::ErrorKind::InvalidData)
    );
    root.replace_current(update, Number::new(350).unwrap())
        .unwrap();
    let mut input = root
        .scan_active_journal(&Provider, bytes.selection(), 273, scratch)
        .unwrap();
    assert!(input.next_frame().unwrap().is_some());
    assert!(input.next_frame().unwrap().is_none());
    let scan = input.finish().unwrap();
    let repaired = scan.repair(&Provider).unwrap();
    assert_eq!(repaired.file().len(), 256);
    assert_eq!(repaired.file().read_at(256, &mut [0]).unwrap(), 0);
    let mut input = root
        .scan_active_journal(&Provider, bytes.selection(), 256, scratch)
        .unwrap();
    assert!(input.next_frame().unwrap().is_some());
    assert!(input.next_frame().unwrap().is_none());
    let scan = input.finish().unwrap();
    assert!(!scan.has_incomplete_tail());
    assert_eq!(scan.summary(), repaired.summary());
    assert!(
        matches!(scan.repair(&Provider),Err(RepairError::Rejected(RecoveryInputError::Io(e))) if e.kind()==io::ErrorKind::InvalidInput)
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::super::super::{tests::Fixture, MAX_PATH_BYTES};
    use super::super::fixture::{self, Bytes, ACCOUNT};
    use super::*;
    use crate::{
        format::MAX_FRAME_BYTES,
        store_paths::{Name, Number},
    };
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
    use td_crypto::Provider;

    fn path(root: &LockedRoot, entry: AccountEntry) -> PathBuf {
        let name = Name::account(ACCOUNT, entry).unwrap();
        root.root
            .directory
            .join(&name, &mut [0; MAX_PATH_BYTES])
            .unwrap()
            .to_owned()
    }
    fn journal_path(root: &LockedRoot) -> PathBuf {
        path(root, AccountEntry::Journal(Number::new(2).unwrap()))
    }
    fn current(root: &LockedRoot, value: Current) {
        let mut bytes = [0; CURRENT_BYTES];
        value.encode(&Provider, &mut bytes).unwrap();
        let path = path(root, AccountEntry::Current);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn setup(fixture: &Fixture) -> (LockedRoot, Bytes) {
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Metadata,
            AccountEntry::Journals,
        ] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
        let bytes = fixture::bytes();
        current(&root, bytes.selection().current());
        tail(&root);
        (root, bytes)
    }
    fn tail(root: &LockedRoot) {
        let mut data = fixture::journal();
        data.extend_from_slice(&[0x5a; 17]);
        fixture::write(root, &data);
    }
    fn scan<'r>(root: &'r LockedRoot, bytes: &Bytes, scratch: &mut [u8]) -> ScannedJournal<'r> {
        let mut input = root
            .scan_active_journal(
                &Provider,
                bytes.selection(),
                273,
                scratch.try_into().unwrap(),
            )
            .unwrap();
        assert!(input.next_frame().unwrap().is_some());
        assert!(input.next_frame().unwrap().is_none());
        input.finish().unwrap()
    }
    #[test]
    fn repair_truncates_only_scanned_tail_and_retains_verified_prefix() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut scratch = vec![0; MAX_FRAME_BYTES];
        let scanned = scan(&root, &bytes, &mut scratch);
        let summary = scanned.summary();
        let repaired = scanned.repair(&Provider).unwrap();
        assert_eq!(repaired.summary(), summary);
        assert_eq!(repaired.file().len(), 256);
        assert_eq!(fs::read(journal_path(&root)).unwrap(), fixture::journal());
        assert_eq!(repaired.file().read_at(256, &mut [0]).unwrap(), 0);
        let scanned = scan(&root, &bytes, &mut scratch);
        assert!(!scanned.has_incomplete_tail());
        assert_eq!(scanned.summary(), repaired.summary());
        assert!(
            matches!(scanned.repair(&Provider), Err(RepairError::Rejected(RecoveryInputError::Io(e))) if e.kind()==io::ErrorKind::InvalidInput)
        );
        assert_eq!(fs::metadata(journal_path(&root)).unwrap().len(), 256);
    }
    #[test]
    fn repair_refuses_stale_corrupt_short_or_missing_current_without_truncation() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut scratch = vec![0; MAX_FRAME_BYTES];
        for case in 0..5 {
            current(&root, bytes.selection().current());
            let scanned = scan(&root, &bytes, &mut scratch);
            let path = path(&root, AccountEntry::Current);
            match case {
                0 => current(
                    &root,
                    Current {
                        generation: 3,
                        ..bytes.selection().current()
                    },
                ),
                1 => fs::write(&path, [0; CURRENT_BYTES]).unwrap(),
                2 => fs::write(&path, [0; CURRENT_BYTES - 1]).unwrap(),
                3 => fs::remove_file(&path).unwrap(),
                _ => fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap(),
            }
            assert!(matches!(
                scanned.repair(&Provider),
                Err(RepairError::Rejected(_))
            ));
            assert_eq!(fs::metadata(journal_path(&root)).unwrap().len(), 273);
        }
    }
    #[test]
    fn repair_refuses_replaced_changed_and_nonprivate_journals() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut scratch = vec![0; MAX_FRAME_BYTES];
        let path = journal_path(&root);
        let extra = path.with_extension("old");
        for case in 0..6 {
            tail(&root);
            let scanned = scan(&root, &bytes, &mut scratch);
            let expected = match case {
                0 => {
                    fs::rename(&path, &extra).unwrap();
                    tail(&root);
                    273
                }
                1 => {
                    OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_len(274)
                        .unwrap();
                    274
                }
                2 => {
                    OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_len(272)
                        .unwrap();
                    272
                }
                3 => {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
                    273
                }
                4 => {
                    fs::hard_link(&path, &extra).unwrap();
                    273
                }
                _ => {
                    fs::rename(&path, &extra).unwrap();
                    std::os::unix::fs::symlink(&extra, &path).unwrap();
                    273
                }
            };
            assert!(matches!(
                scanned.repair(&Provider),
                Err(RepairError::Rejected(_))
            ));
            assert_eq!(fs::metadata(&path).unwrap().len(), expected);
            if extra.exists() {
                assert_eq!(fs::metadata(&extra).unwrap().len(), 273);
                fs::remove_file(&extra).unwrap();
            }
            fs::remove_file(&path).unwrap();
        }
    }
    struct Fault<'a> {
        point: u8,
        calls: &'a mut [usize; 3],
    }
    impl Operations for Fault<'_> {
        fn truncate(&mut self, file: &File, length: u64) -> io::Result<()> {
            self.calls[0] += 1;
            if self.point != 0 {
                file.set_len(length)?;
            }
            if self.point <= 1 {
                return Err(io::ErrorKind::StorageFull.into());
            }
            Ok(())
        }
        fn sync(&mut self, file: &File) -> io::Result<()> {
            self.calls[1] += 1;
            if self.point != 2 {
                file.sync_all()?;
            }
            if matches!(self.point, 2 | 3) {
                return Err(io::ErrorKind::StorageFull.into());
            }
            if self.point == 5 {
                file.set_len(257)?;
            }
            Ok(())
        }
        fn read(&mut self, file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
            self.calls[2] += 1;
            if self.point == 4 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            file.read_at(bytes, offset)
        }
    }
    #[test]
    fn repair_errors_preserve_effect_stage_without_retry_or_cleanup() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut scratch = vec![0; MAX_FRAME_BYTES];
        for point in 0..6 {
            tail(&root);
            let scanned = scan(&root, &bytes, &mut scratch);
            let mut calls = [0; 3];
            let error = scanned
                .repair_using(
                    &Provider,
                    Fault {
                        point,
                        calls: &mut calls,
                    },
                )
                .unwrap_err();
            match point {
                0 | 1 => assert!(
                    matches!(error,RepairError::TruncateAttempted(e) if e.kind()==io::ErrorKind::StorageFull)
                ),
                2 | 3 => assert!(
                    matches!(error,RepairError::Truncated(e) if e.kind()==io::ErrorKind::StorageFull)
                ),
                4 => assert!(
                    matches!(error,RepairError::Synced(e) if e.kind()==io::ErrorKind::Interrupted)
                ),
                _ => assert!(
                    matches!(error,RepairError::Synced(e) if e.kind()==io::ErrorKind::InvalidData)
                ),
            }
            assert_eq!(
                calls,
                match point {
                    0 | 1 => [1, 0, 0],
                    2 | 3 | 5 => [1, 1, 0],
                    _ => [1, 1, 1],
                }
            );
            let actual = fs::read(journal_path(&root)).unwrap();
            assert_eq!(
                actual.len(),
                match point {
                    0 => 273,
                    5 => 257,
                    _ => 256,
                }
            );
            assert_eq!(actual.get(..256).unwrap(), fixture::journal());
        }
    }
}
