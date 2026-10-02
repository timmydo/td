//! FORMAT/CURRENT/manifest binding from private files, not complete account recovery.
use super::{LockedRoot, StoreReader};
use crate::{
    format::{
        bindings::Selection,
        container::{Current, Error as ContainerError, StoreIdentity},
        CURRENT_BYTES, FORMAT_BYTES, MAX_MANIFEST_BYTES,
    },
    ids::AccountId,
    ports::Crypto,
    store_paths::{AccountEntry, Number},
};
use std::io;

const MAX_METADATA_READ_CALLS: usize = 64;

/// Caller-owned scratch. Its manifest stays borrowed while Selection is live.
pub struct SelectionScratch {
    format: [u8; FORMAT_BYTES],
    current: [u8; CURRENT_BYTES],
    manifest: [u8; MAX_MANIFEST_BYTES],
}
impl SelectionScratch {
    pub const fn new() -> Self {
        Self {
            format: [0; FORMAT_BYTES],
            current: [0; CURRENT_BYTES],
            manifest: [0; MAX_MANIFEST_BYTES],
        }
    }
}
impl Default for SelectionScratch {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionStage {
    Format,
    Current,
    Manifest,
    Binding,
}
#[derive(Debug)]
pub enum SelectionError {
    Io(SelectionStage, io::Error),
    Container(SelectionStage, ContainerError),
}
impl std::fmt::Display for SelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(stage, e) => write!(f, "selected metadata {stage:?} I/O: {e}"),
            Self::Container(stage, e) => write!(f, "selected metadata {stage:?} encoding: {e}"),
        }
    }
}
impl std::error::Error for SelectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(_, e) => Some(e),
            Self::Container(_, e) => Some(e),
        }
    }
}
impl LockedRoot {
    /// Run during quiescent recovery or under the actual selection barrier.
    /// Tables, journals, final rows and pins still require separate validation.
    pub fn load_selection<'a>(
        &self,
        crypto: &impl Crypto,
        account: AccountId,
        scratch: &'a mut SelectionScratch,
    ) -> Result<Selection<'a>, SelectionError> {
        let format_bytes = load(
            SelectionStage::Format,
            self.open_format(),
            &mut scratch.format,
        )?;
        let store = StoreIdentity::decode(crypto, format_bytes)
            .map_err(|e| SelectionError::Container(SelectionStage::Format, e))?;
        let current_bytes = load(
            SelectionStage::Current,
            self.open_account_file(account, AccountEntry::Current, CURRENT_BYTES as u64),
            &mut scratch.current,
        )?;
        let current = Current::decode(crypto, current_bytes)
            .map_err(|e| SelectionError::Container(SelectionStage::Current, e))?;
        if current.account != account || current.epoch != store.epoch {
            return Err(SelectionError::Container(
                SelectionStage::Current,
                ContainerError::Format(crate::format::Error::InvalidValue),
            ));
        }
        let generation = Number::new(current.generation).map_err(|_| {
            SelectionError::Container(
                SelectionStage::Current,
                ContainerError::Format(crate::format::Error::InvalidValue),
            )
        })?;
        let manifest = load(
            SelectionStage::Manifest,
            self.open_account_file(
                account,
                AccountEntry::Manifest(generation),
                MAX_MANIFEST_BYTES as u64,
            ),
            &mut scratch.manifest,
        )?;
        crate::format::manifest::Manifest::decode(crypto, manifest)
            .map_err(|e| SelectionError::Container(SelectionStage::Manifest, e))?;
        Selection::decode(crypto, account, format_bytes, current_bytes, manifest)
            .map_err(|e| SelectionError::Container(SelectionStage::Binding, e))
    }
}
fn load<'a>(
    stage: SelectionStage,
    reader: io::Result<StoreReader<'_>>,
    output: &'a mut [u8],
) -> Result<&'a [u8], SelectionError> {
    let reader = reader.map_err(|e| SelectionError::Io(stage, e))?;
    let length =
        read_file(reader, output, StoreReader::read).map_err(|e| SelectionError::Io(stage, e))?;
    output
        .get(..length)
        .ok_or_else(|| SelectionError::Io(stage, io::ErrorKind::InvalidData.into()))
}
fn read_file<'r>(
    mut reader: StoreReader<'r>,
    output: &mut [u8],
    mut read: impl FnMut(&mut StoreReader<'r>, &mut [u8]) -> io::Result<usize>,
) -> io::Result<usize> {
    let length = usize::try_from(reader.len()).map_err(|_| io::ErrorKind::InvalidData)?;
    let output = output
        .get_mut(..length)
        .ok_or(io::ErrorKind::InvalidInput)?;
    let mut offset = 0;
    let mut calls = 0;
    while offset < length {
        if calls == MAX_METADATA_READ_CALLS {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        calls += 1;
        let count = read(
            &mut reader,
            output.get_mut(offset..).ok_or(io::ErrorKind::InvalidData)?,
        )?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        offset = offset
            .checked_add(count)
            .filter(|offset| *offset <= length)
            .ok_or(io::ErrorKind::InvalidData)?;
    }
    drop(reader.finish()?);
    Ok(length)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod fixture {
    use super::*;
    use crate::store_paths::{Name, RootEntry};
    use std::{fs, os::unix::fs::PermissionsExt};
    pub(super) const ACCOUNT: AccountId = AccountId::from_bytes([0x33; 16]);
    pub(super) const FORMAT: &str = include_str!("../../tests/fixtures/format-v1/format.hex");
    pub(super) const CURRENT: &str = include_str!("../../tests/fixtures/format-v1/current.hex");
    pub(super) const MANIFEST: &str = include_str!("../../tests/fixtures/format-v1/manifest.hex");
    pub(super) fn hex(text: &str) -> Vec<u8> {
        let text: String = text.split_whitespace().collect();
        let (pairs, rest) = text.as_bytes().as_chunks::<2>();
        assert!(rest.is_empty());
        pairs
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    pub(super) fn write(root: &LockedRoot, name: Name, bytes: &[u8]) {
        let mut buffer = [0; super::super::MAX_PATH_BYTES];
        let path = root.root.directory.join(&name, &mut buffer).unwrap();
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    pub(super) fn current() -> Name {
        Name::account(ACCOUNT, AccountEntry::Current).unwrap()
    }
    pub(super) fn manifest(generation: u64) -> Name {
        Name::account(
            ACCOUNT,
            AccountEntry::Manifest(Number::new(generation).unwrap()),
        )
        .unwrap()
    }
    pub(super) fn prepare(root: &LockedRoot) {
        // The enclosing fixture supplies accounts/, as the allocation fixture does.
        for entry in [
            AccountEntry::Root,
            AccountEntry::Metadata,
            AccountEntry::Checkpoints,
            AccountEntry::Checkpoint(Number::new(1).unwrap()),
            AccountEntry::Checkpoint(Number::new(2).unwrap()),
        ] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
        write(root, Name::root(RootEntry::Format).unwrap(), &hex(FORMAT));
        write(root, current(), &hex(CURRENT));
        write(root, manifest(1), &hex(MANIFEST));
    }
}
#[cfg(test)]
pub(super) fn prepare_probe(root: &LockedRoot) {
    fixture::prepare(root);
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) fn probe(root: &LockedRoot, scratch: &mut SelectionScratch) {
    let selection = root
        .load_selection(&td_crypto::Provider, fixture::ACCOUNT, scratch)
        .unwrap();
    assert_eq!(selection.current().generation, 1);
    assert_eq!(selection.current().account, fixture::ACCOUNT);
    let missing = AccountId::from_bytes([0x34; 16]);
    assert!(
        matches!(root.load_selection(&td_crypto::Provider, missing, scratch),
        Err(SelectionError::Io(SelectionStage::Current, e)) if e.kind() == io::ErrorKind::NotFound)
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::tests::Fixture;
    use super::{fixture::*, *};
    use crate::{
        ids::StoreEpoch,
        store_paths::{Name, RootEntry},
    };
    use td_crypto::{Digest, Provider};
    fn setup(fixture: &Fixture) -> LockedRoot {
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        prepare(&root);
        root
    }
    #[test]
    fn private_selection_binds_literal_metadata_and_reuses_scratch() {
        let fixture = Fixture::new();
        let root = setup(&fixture);
        let mut scratch = SelectionScratch::new();
        for _ in 0..2 {
            assert_eq!(
                root.load_selection(&Provider, ACCOUNT, &mut scratch)
                    .unwrap(),
                Selection::decode(
                    &Provider,
                    ACCOUNT,
                    &hex(FORMAT),
                    &hex(CURRENT),
                    &hex(MANIFEST)
                )
                .unwrap()
            );
        }
        probe(&root, &mut scratch);
    }
    #[test]
    fn selection_refuses_corrupt_truncated_and_oversized_files_by_stage() {
        let fixture = Fixture::new();
        let root = setup(&fixture);
        let mut scratch = SelectionScratch::new();
        for (name, literal, limit, stage) in [
            (
                Name::root(RootEntry::Format).unwrap(),
                FORMAT,
                FORMAT_BYTES,
                SelectionStage::Format,
            ),
            (current(), CURRENT, CURRENT_BYTES, SelectionStage::Current),
            (
                manifest(1),
                MANIFEST,
                MAX_MANIFEST_BYTES,
                SelectionStage::Manifest,
            ),
        ] {
            let bytes = hex(literal);
            write(&root, name.clone(), bytes.get(..bytes.len() - 1).unwrap());
            assert!(
                matches!(root.load_selection(&Provider, ACCOUNT, &mut scratch),
                Err(SelectionError::Container(s, _)) if s == stage)
            );
            let mut corrupt = bytes.clone();
            *corrupt.last_mut().unwrap() ^= 1;
            write(&root, name.clone(), &corrupt);
            assert!(
                matches!(root.load_selection(&Provider, ACCOUNT, &mut scratch),
                Err(SelectionError::Container(s, ContainerError::Checksum)) if s == stage)
            );
            write(&root, name.clone(), &vec![0; limit + 1]);
            assert!(
                matches!(root.load_selection(&Provider, ACCOUNT, &mut scratch),
                Err(SelectionError::Io(s, e)) if s == stage && e.kind() == io::ErrorKind::InvalidData)
            );
            write(&root, name.clone(), &bytes);
        }
    }
    #[test]
    fn selection_checks_current_identity_before_following_and_never_scans() {
        let fixture = Fixture::new();
        let root = setup(&fixture);
        let mut scratch = SelectionScratch::new();
        let original = Current::decode(&Provider, &hex(CURRENT)).unwrap();
        let mut encoded = [0; CURRENT_BYTES];
        for value in [
            Current {
                account: AccountId::from_bytes([9; 16]),
                generation: 3,
                ..original
            },
            Current {
                epoch: StoreEpoch::from_bytes([9; 16]),
                generation: 3,
                ..original
            },
        ] {
            value.encode(&Provider, &mut encoded).unwrap();
            write(&root, current(), &encoded);
            assert!(matches!(
                root.load_selection(&Provider, ACCOUNT, &mut scratch),
                Err(SelectionError::Container(
                    SelectionStage::Current,
                    ContainerError::Format(crate::format::Error::InvalidValue)
                ))
            ));
        }
        // A valid newer manifest cannot replace the missing selected manifest.
        let mut newer = hex(MANIFEST);
        newer
            .get_mut(48..56)
            .unwrap()
            .copy_from_slice(&2u64.to_le_bytes());
        let end = newer.len() - 32;
        let mut hasher = Provider.sha256().unwrap();
        hasher.update(newer.get(..end).unwrap()).unwrap();
        let digest = hasher.finish().unwrap();
        newer.get_mut(end..).unwrap().copy_from_slice(&digest);
        assert_eq!(
            crate::format::manifest::Manifest::decode(&Provider, &newer)
                .unwrap()
                .header()
                .generation,
            2
        );
        write(&root, manifest(2), &newer);
        let mut path = [0; super::super::MAX_PATH_BYTES];
        std::fs::remove_file(root.root.directory.join(&manifest(1), &mut path).unwrap()).unwrap();
        write(&root, current(), &hex(CURRENT));
        assert!(
            matches!(root.load_selection(&Provider, ACCOUNT, &mut scratch),
            Err(SelectionError::Io(SelectionStage::Manifest, e)) if e.kind() == io::ErrorKind::NotFound)
        );
        write(&root, manifest(1), &hex(MANIFEST));
        let wrong_digest = Current {
            manifest_digest: [0; 32],
            ..original
        };
        wrong_digest.encode(&Provider, &mut encoded).unwrap();
        write(&root, current(), &encoded);
        assert!(matches!(
            root.load_selection(&Provider, ACCOUNT, &mut scratch),
            Err(SelectionError::Container(
                SelectionStage::Binding,
                ContainerError::Checksum
            ))
        ));
    }
    #[test]
    fn metadata_short_reads_have_a_fixed_attempt_bound() {
        let fixture = Fixture::new();
        let root = setup(&fixture);
        let mut bytes = [0; FORMAT_BYTES];
        let mut calls = 0;
        assert_eq!(
            read_file(root.open_format().unwrap(), &mut bytes, |reader, output| {
                calls += 1;
                reader.read(output.get_mut(..1).unwrap())
            })
            .unwrap_err()
            .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(calls, MAX_METADATA_READ_CALLS);
        let mut calls = 0;
        assert_eq!(
            read_file(root.open_format().unwrap(), &mut bytes, |reader, output| {
                calls += 1;
                let count = output.len().min(2);
                reader.read(output.get_mut(..count).unwrap())
            })
            .unwrap(),
            FORMAT_BYTES
        );
        assert_eq!(calls, FORMAT_BYTES / 2);
        assert_eq!(bytes.as_slice(), hex(FORMAT));
        assert_eq!(
            read_file(root.open_format().unwrap(), &mut bytes, |_, _| {
                Err(io::ErrorKind::Interrupted.into())
            })
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
    }
}
