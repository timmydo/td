//! Incremental table input; rows remain provisional until completion and binding.
use super::{
    input::{fill_exact as fill, fill_exact_using as fill_using},
    CompleteFile, LockedRoot, StoreReader,
};
use crate::{
    format::{
        bindings::Selection,
        container::Error as ContainerError,
        table::{record_extent, Record, MAX_RECORD_BYTES, RECORD_PREFIX_BYTES},
        table_stream::{Summary, Verifier},
        Table, TABLE_HEADER_BYTES,
    },
    ports::Crypto,
    store_paths::{AccountEntry, Number},
};
use std::io;

#[path = "table/replay.rs"]
mod replay;
#[cfg(test)]
pub(super) use replay::probe as probe_replay;
pub use replay::{CompleteReplay, Error as TableReplayError, TableReplay};

#[path = "table/lookup.rs"]
mod lookup;
#[cfg(test)]
pub(super) use lookup::probe as probe_lookup;
pub use lookup::{CompleteLookup, LookupError, TableLookup};

const MAX_TABLE_READ_CALLS: usize = 64;
#[derive(Debug)]
pub enum TableInputError {
    Io(io::Error),
    Container(ContainerError),
}
impl From<io::Error> for TableInputError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<ContainerError> for TableInputError {
    fn from(error: ContainerError) -> Self {
        Self::Container(error)
    }
}
impl From<crate::format::Error> for TableInputError {
    fn from(error: crate::format::Error) -> Self {
        Self::Container(error.into())
    }
}
impl std::fmt::Display for TableInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "table input I/O: {e}"),
            Self::Container(e) => write!(f, "table input encoding: {e}"),
        }
    }
}
impl std::error::Error for TableInputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Container(e) => Some(e),
        }
    }
}

pub struct TableInput<'r, 'c, 'm, 'b, C: Crypto> {
    file: StoreReader<'r>,
    verifier: Verifier<'c, C>,
    selection: Selection<'m>,
    crypto: &'c C,
    table: Table,
    scratch: &'b mut [u8; MAX_RECORD_BYTES],
    remaining: u64,
    failed: bool,
}
impl LockedRoot {
    /// The caller holds its recovery/view barrier and admits max_bytes/read work.
    pub fn open_table<'r, 'c, 'm, 'b, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'m>,
        table: Table,
        max_bytes: u64,
        scratch: &'b mut [u8; MAX_RECORD_BYTES],
    ) -> Result<TableInput<'r, 'c, 'm, 'b, C>, TableInputError> {
        let descriptor = selection.manifest().table(table)?;
        if descriptor.file_bytes > max_bytes {
            return Err(io::Error::from(io::ErrorKind::InvalidInput).into());
        }
        let generation = Number::new(selection.current().generation)
            .map_err(|_| crate::format::Error::InvalidValue)?;
        let mut file = self.open_account_file(
            selection.current().account,
            AccountEntry::Table(generation, table),
            descriptor.file_bytes,
        )?;
        if file.len() != descriptor.file_bytes {
            return Err(io::Error::from(io::ErrorKind::InvalidData).into());
        }
        let mut header_bytes = [0; TABLE_HEADER_BYTES];
        let mut attempts = MAX_TABLE_READ_CALLS;
        fill(&mut file, &mut header_bytes, &mut attempts)?;
        let verifier = Verifier::new(crypto, &header_bytes)?;
        let header = verifier.header();
        let manifest = selection.manifest().header();
        if header.table != table
            || header.account != manifest.account
            || header.epoch != manifest.epoch
            || header.generation != manifest.generation
            || header.through != manifest.through
            || header.record_count != descriptor.record_count
            || header.file_bytes()? != descriptor.file_bytes
        {
            return Err(crate::format::Error::InvalidValue.into());
        }
        Ok(TableInput {
            file,
            verifier,
            selection,
            crypto,
            table,
            scratch,
            remaining: header.record_count,
            failed: false,
        })
    }
}
impl<'r, C: Crypto> TableInput<'r, '_, '_, '_, C> {
    pub fn is_failed(&self) -> bool {
        self.failed
    }
    /// One provisional record; None means no declared rows remain, not completion.
    pub fn next_record(&mut self) -> Result<Option<Record<'_>>, TableInputError> {
        self.next_record_using(StoreReader::read)
    }
    fn next_record_using(
        &mut self,
        mut read: impl FnMut(&mut StoreReader<'r>, &mut [u8]) -> io::Result<usize>,
    ) -> Result<Option<Record<'_>>, TableInputError> {
        if self.failed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe).into());
        }
        if self.remaining == 0 {
            return Ok(None);
        }
        self.failed = true;
        let mut attempts = MAX_TABLE_READ_CALLS;
        let prefix = self
            .scratch
            .get_mut(..RECORD_PREFIX_BYTES)
            .ok_or(crate::format::Error::Limit)?;
        fill_using(&mut self.file, prefix, &mut attempts, &mut read)?;
        let length = record_extent(prefix)?;
        let body_length = length
            .checked_sub(RECORD_PREFIX_BYTES)
            .ok_or(crate::format::Error::InvalidValue)?;
        let remaining = self
            .file
            .len()
            .checked_sub(self.file.position())
            .ok_or(crate::format::Error::InvalidValue)?;
        if body_length as u64 > remaining {
            return Err(crate::format::Error::InvalidValue.into());
        }
        let bytes = self
            .scratch
            .get_mut(..length)
            .ok_or(crate::format::Error::Limit)?;
        fill_using(
            &mut self.file,
            bytes
                .get_mut(RECORD_PREFIX_BYTES..)
                .ok_or(crate::format::Error::Limit)?,
            &mut attempts,
            &mut read,
        )?;
        let record = self.verifier.push(bytes)?;
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or(crate::format::Error::InvalidValue)?;
        self.failed = false;
        Ok(Some(record))
    }
}
impl<'r, C: Crypto> TableInput<'r, '_, '_, '_, C> {
    pub fn finish(self) -> Result<CompleteTable<'r>, TableInputError> {
        if self.failed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe).into());
        }
        if self.remaining != 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput).into());
        }
        let summary = self.verifier.finish()?;
        let file = self.file.finish()?;
        self.selection
            .check_table(self.crypto, self.table, summary)?;
        Ok(CompleteTable { file, summary })
    }
}
/// One table bound to its supplied Selection; whole-graph validity/pins remain external.
#[derive(Debug)]
pub struct CompleteTable<'a> {
    file: CompleteFile<'a>,
    summary: Summary,
}
impl CompleteTable<'_> {
    pub fn file(&self) -> &CompleteFile<'_> {
        &self.file
    }
    pub fn summary(&self) -> Summary {
        self.summary
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod fixture {
    use super::*;
    use crate::{ids::AccountId, store_paths::Name};
    use std::{fs, os::unix::fs::PermissionsExt};
    pub(super) const ACCOUNT: AccountId = AccountId::from_bytes([0x33; 16]);
    pub(super) const TABLES: [&str; 11] = [
        include_str!("../../tests/fixtures/format-v1/populated-blob-table.hex"),
        include_str!("../../tests/fixtures/format-v1/checkpoint-table-2.hex"),
        include_str!("../../tests/fixtures/format-v1/checkpoint-table-3.hex"),
        include_str!("../../tests/fixtures/format-v1/checkpoint-table-4.hex"),
        include_str!("../../tests/fixtures/format-v1/checkpoint-table-5.hex"),
        include_str!("../../tests/fixtures/format-v1/checkpoint-table-6.hex"),
        include_str!("../../tests/fixtures/format-v1/checkpoint-table-7.hex"),
        include_str!("../../tests/fixtures/format-v1/checkpoint-table-8.hex"),
        include_str!("../../tests/fixtures/format-v1/checkpoint-table-9.hex"),
        include_str!("../../tests/fixtures/format-v1/checkpoint-table-10.hex"),
        include_str!("../../tests/fixtures/format-v1/checkpoint-table-11.hex"),
    ];
    pub(super) fn hex(text: &str) -> Vec<u8> {
        let text: String = text.split_whitespace().collect();
        let (pairs, rest) = text.as_bytes().as_chunks::<2>();
        assert!(rest.is_empty());
        pairs
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    pub(super) fn name(table: Table) -> Name {
        Name::account(ACCOUNT, AccountEntry::Table(Number::new(2).unwrap(), table)).unwrap()
    }
    pub(super) fn write(root: &LockedRoot, table: Table, bytes: &[u8]) {
        let mut buffer = [0; super::super::MAX_PATH_BYTES];
        let path = root.root.directory.join(&name(table), &mut buffer).unwrap();
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    pub(crate) struct Bytes {
        format: Vec<u8>,
        pub(super) current: Vec<u8>,
        pub(super) manifest: Vec<u8>,
    }
    impl Bytes {
        pub(super) fn selection(&self) -> Selection<'_> {
            Selection::decode(
                &td_crypto::Provider,
                ACCOUNT,
                &self.format,
                &self.current,
                &self.manifest,
            )
            .unwrap()
        }
    }
    pub(crate) fn prepare(root: &LockedRoot) -> Bytes {
        // The enclosing fixture supplies account/checkpoint directories.
        for (index, text) in TABLES.iter().enumerate() {
            write(
                root,
                Table::from_tag((index + 1) as u16).unwrap(),
                &hex(text),
            );
        }
        Bytes {
            format: hex(include_str!("../../tests/fixtures/format-v1/format.hex")),
            current: hex(include_str!(
                "../../tests/fixtures/format-v1/current-history.hex"
            )),
            manifest: hex(include_str!(
                "../../tests/fixtures/format-v1/manifest-history.hex"
            )),
        }
    }
}
#[cfg(test)]
pub(super) use fixture::{prepare as prepare_probe, Bytes as ProbeBytes};
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) fn probe(root: &LockedRoot, bytes: &ProbeBytes, scratch: &mut [u8; MAX_RECORD_BYTES]) {
    let selection = bytes.selection();
    for tag in 1..=11 {
        let table = Table::from_tag(tag).unwrap();
        let mut input = root
            .open_table(&td_crypto::Provider, selection, table, 225, scratch)
            .unwrap();
        if table == Table::Blobs {
            let row = input.next_record().unwrap().unwrap();
            assert_eq!(row.key_bytes(), &[0x44; 16]);
        }
        assert!(input.next_record().unwrap().is_none());
        let complete = input.finish().unwrap();
        assert_eq!(complete.summary().header().table, table);
        assert_eq!(
            complete.file().len(),
            selection.manifest().table(table).unwrap().file_bytes
        );
    }
    assert!(
        matches!(root.open_table(&td_crypto::Provider, selection, Table::Blobs, 224, scratch),
        Err(TableInputError::Io(e)) if e.kind() == io::ErrorKind::InvalidInput)
    );
    let input = root
        .open_table(&td_crypto::Provider, selection, Table::Blobs, 225, scratch)
        .unwrap();
    assert!(
        matches!(input.finish(), Err(TableInputError::Io(e)) if e.kind() == io::ErrorKind::InvalidInput)
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::tests::Fixture;
    use super::{fixture::*, *};
    use td_crypto::{Digest, Provider};
    fn setup(fixture: &Fixture) -> (LockedRoot, Bytes) {
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Metadata,
            AccountEntry::Checkpoints,
            AccountEntry::Checkpoint(Number::new(2).unwrap()),
        ] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
        let bytes = prepare(&root);
        (root, bytes)
    }
    fn original() -> Vec<u8> {
        hex(TABLES.first().unwrap())
    }
    fn rehash(bytes: &mut [u8]) {
        let end = bytes.len() - 32;
        let mut hash = Provider.sha256().unwrap();
        hash.update(bytes.get(..end).unwrap()).unwrap();
        bytes
            .get_mut(end..)
            .unwrap()
            .copy_from_slice(&hash.finish().unwrap());
    }
    fn digest(bytes: &[u8]) -> [u8; 32] {
        let mut hash = Provider.sha256().unwrap();
        hash.update(bytes).unwrap();
        hash.finish().unwrap()
    }
    pub(super) fn two_rows(root: &LockedRoot, bytes: &mut Bytes) -> Vec<u8> {
        use crate::format::{container::Current, manifest, table::TableHeader, Table, TABLE_COUNT};
        let original = original();
        let mut second = original.get(TABLE_HEADER_BYTES..).unwrap().to_vec();
        *second.get_mut(RECORD_PREFIX_BYTES).unwrap() = 0x45;
        rehash(&mut second);
        let mut table = original.clone();
        table.extend_from_slice(&second);
        let header = TableHeader {
            record_count: 2,
            payload_bytes: 226,
            ..TableHeader::decode(&Provider, original.get(..TABLE_HEADER_BYTES).unwrap()).unwrap()
        };
        header
            .encode(&Provider, table.get_mut(..TABLE_HEADER_BYTES).unwrap())
            .unwrap();
        let selection = bytes.selection();
        let manifest = selection.manifest();
        let mut descriptors: [manifest::TableDescriptor; TABLE_COUNT] =
            std::array::from_fn(|index| {
                manifest
                    .table(Table::from_tag((index + 1) as u16).unwrap())
                    .unwrap()
            });
        let descriptor = descriptors.first_mut().unwrap();
        descriptor.record_count = 2;
        descriptor.file_bytes = table.len() as u64;
        descriptor.digest = digest(&table);
        let history: Vec<_> = (0..manifest.history_count())
            .map(|index| manifest.history(index).unwrap())
            .collect();
        let mut encoded = vec![0; crate::format::MAX_MANIFEST_BYTES];
        let length = manifest::encode(
            &Provider,
            manifest.header(),
            &descriptors,
            &history,
            &mut encoded,
        )
        .unwrap();
        encoded.truncate(length);
        bytes.manifest = encoded;
        let current = Current {
            manifest_digest: digest(&bytes.manifest),
            ..Current::decode(&Provider, &bytes.current).unwrap()
        };
        current.encode(&Provider, &mut bytes.current).unwrap();
        write(root, Table::Blobs, &table);
        table
    }
    #[test]
    fn selected_table_inputs_stream_literal_rows_and_complete_all_tags() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut scratch = [0; MAX_RECORD_BYTES];
        probe(&root, &bytes, &mut scratch);
    }
    #[test]
    fn table_open_requires_selected_extent_and_header_identity() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let selection = bytes.selection();
        let mut scratch = [0; MAX_RECORD_BYTES];
        let original = original();
        for length in [0, 111, 224, 226] {
            let mut changed = original.clone();
            changed.resize(length, 0);
            write(&root, Table::Blobs, &changed);
            assert!(
                matches!(root.open_table(&Provider, selection, Table::Blobs, 225, &mut scratch),
                Err(TableInputError::Io(e)) if e.kind() == io::ErrorKind::InvalidData)
            );
        }
        let mut bytes = bytes;
        let table = two_rows(&root, &mut bytes);
        let selection = bytes.selection();
        let header = crate::format::table::TableHeader::decode(
            &Provider,
            table.get(..TABLE_HEADER_BYTES).unwrap(),
        )
        .unwrap();
        // Each header is valid on its own and differs in exactly one compared field.
        for changed_header in [
            crate::format::table::TableHeader {
                table: Table::Emails,
                ..header
            },
            crate::format::table::TableHeader {
                account: crate::ids::AccountId::from_bytes([9; 16]),
                ..header
            },
            crate::format::table::TableHeader {
                epoch: crate::ids::StoreEpoch::from_bytes([9; 16]),
                ..header
            },
            crate::format::table::TableHeader {
                generation: 3,
                ..header
            },
            crate::format::table::TableHeader {
                through: crate::format::Sequence::from_u64(2),
                ..header
            },
            crate::format::table::TableHeader {
                record_count: 1,
                ..header
            },
            crate::format::table::TableHeader {
                payload_bytes: 227,
                ..header
            },
        ] {
            let mut changed = table.clone();
            changed_header
                .encode(&Provider, changed.get_mut(..TABLE_HEADER_BYTES).unwrap())
                .unwrap();
            write(&root, Table::Blobs, &changed);
            assert!(matches!(
                root.open_table(
                    &Provider,
                    selection,
                    Table::Blobs,
                    table.len() as u64,
                    &mut scratch
                ),
                Err(TableInputError::Container(ContainerError::Format(
                    crate::format::Error::InvalidValue
                )))
            ));
        }
    }
    #[test]
    fn table_row_failures_retire_and_prevent_completion() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let selection = bytes.selection();
        let mut scratch = [0; MAX_RECORD_BYTES];
        for mode in 0..4 {
            write(&root, Table::Blobs, &original());
            let mut input = root
                .open_table(&Provider, selection, Table::Blobs, 225, &mut scratch)
                .unwrap();
            let mut changed = original();
            match mode {
                0 => {
                    changed.truncate(TABLE_HEADER_BYTES);
                }
                1 => {
                    *changed.last_mut().unwrap() ^= 1;
                }
                2 => {
                    changed
                        .get_mut(TABLE_HEADER_BYTES..TABLE_HEADER_BYTES + 4)
                        .unwrap()
                        .copy_from_slice(&u32::MAX.to_le_bytes());
                }
                _ => {
                    changed
                        .get_mut(TABLE_HEADER_BYTES + 4..TABLE_HEADER_BYTES + 8)
                        .unwrap()
                        .copy_from_slice(&50u32.to_le_bytes());
                }
            }
            write(&root, Table::Blobs, &changed);
            if mode == 3 {
                assert!(matches!(
                    input.next_record(),
                    Err(TableInputError::Container(ContainerError::Format(
                        crate::format::Error::InvalidValue
                    )))
                ));
                assert_eq!(
                    input.file.position(),
                    (TABLE_HEADER_BYTES + RECORD_PREFIX_BYTES) as u64
                );
            } else {
                assert!(input.next_record().is_err());
            }
            assert!(input.is_failed());
            assert!(
                matches!(input.next_record(), Err(TableInputError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe)
            );
            assert!(
                matches!(input.finish(), Err(TableInputError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe)
            );
        }
    }
    #[test]
    fn table_completion_requires_physical_extent_and_selected_digest() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let selection = bytes.selection();
        let mut scratch = [0; MAX_RECORD_BYTES];
        for changed_length in [0, 226] {
            write(&root, Table::Blobs, &original());
            let mut input = root
                .open_table(&Provider, selection, Table::Blobs, 225, &mut scratch)
                .unwrap();
            assert!(input.next_record().unwrap().is_some());
            let mut changed = original();
            changed.resize(changed_length, 0);
            write(&root, Table::Blobs, &changed);
            assert!(
                matches!(input.finish(), Err(TableInputError::Io(e)) if e.kind() == io::ErrorKind::InvalidData)
            );
        }
        let mut changed = original();
        // Valid changed blob key and record checksum, with the original manifest digest.
        *changed
            .get_mut(TABLE_HEADER_BYTES + RECORD_PREFIX_BYTES)
            .unwrap() ^= 1;
        rehash(changed.get_mut(TABLE_HEADER_BYTES..).unwrap());
        write(&root, Table::Blobs, &changed);
        let mut input = root
            .open_table(&Provider, selection, Table::Blobs, 225, &mut scratch)
            .unwrap();
        assert!(input.next_record().unwrap().is_some());
        assert!(input.next_record().unwrap().is_none());
        assert!(matches!(
            input.finish(),
            Err(TableInputError::Container(ContainerError::Checksum))
        ));
    }
    #[test]
    fn table_input_reuses_scratch_and_shares_prefix_body_attempts() {
        let fixture = Fixture::new();
        let (root, mut bytes) = setup(&fixture);
        let table = two_rows(&root, &mut bytes);
        let selection = bytes.selection();
        let mut scratch = [0; MAX_RECORD_BYTES];
        let mut input = root
            .open_table(
                &Provider,
                selection,
                Table::Blobs,
                table.len() as u64,
                &mut scratch,
            )
            .unwrap();
        for first in [0x44, 0x45] {
            let row = input.next_record().unwrap().unwrap();
            assert_eq!(row.key_bytes().first(), Some(&first));
        }
        assert!(input.next_record().unwrap().is_none());
        assert_eq!(input.finish().unwrap().summary().header().record_count, 2);
        let mut input = root
            .open_table(
                &Provider,
                selection,
                Table::Blobs,
                table.len() as u64,
                &mut scratch,
            )
            .unwrap();
        let mut calls = 0;
        assert!(matches!(input.next_record_using(|file, output| {
            calls += 1;
            file.read(output.get_mut(..1).unwrap())
        }), Err(TableInputError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock));
        assert_eq!(calls, MAX_TABLE_READ_CALLS);
        assert_eq!(
            input.file.position(),
            (TABLE_HEADER_BYTES + MAX_TABLE_READ_CALLS) as u64
        );
        assert!(input.is_failed());
        assert!(
            matches!(input.next_record(), Err(TableInputError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe)
        );
        assert!(
            matches!(input.finish(), Err(TableInputError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe)
        );
    }
    #[test]
    fn table_fill_short_reads_and_attempts_are_bounded() {
        let fixture = Fixture::new();
        let (root, _) = setup(&fixture);
        let mut scratch = [0; TABLE_HEADER_BYTES];
        let open = || {
            root.open_account_file(
                ACCOUNT,
                AccountEntry::Table(Number::new(2).unwrap(), Table::Blobs),
                225,
            )
            .unwrap()
        };
        let mut file = open();
        let mut attempts = MAX_TABLE_READ_CALLS;
        let mut calls = 0;
        assert_eq!(
            fill_using(&mut file, &mut scratch, &mut attempts, |file, output| {
                calls += 1;
                file.read(output.get_mut(..1).unwrap())
            })
            .unwrap_err()
            .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(attempts, 0);
        assert_eq!(calls, MAX_TABLE_READ_CALLS);
        let mut file = open();
        let mut attempts = MAX_TABLE_READ_CALLS;
        fill_using(&mut file, &mut scratch, &mut attempts, |file, output| {
            let count = output.len().min(2);
            file.read(output.get_mut(..count).unwrap())
        })
        .unwrap();
        assert_eq!(file.position(), TABLE_HEADER_BYTES as u64);
        assert_eq!(attempts, MAX_TABLE_READ_CALLS - TABLE_HEADER_BYTES / 2);
        assert_eq!(
            scratch.as_slice(),
            original().get(..TABLE_HEADER_BYTES).unwrap()
        );
    }
}
