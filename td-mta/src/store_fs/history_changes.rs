//! Selected immutable history using operation scratch and retained change slots.
use super::{change_input::ChangesInput, CompleteFile, HistoryInputError, LockedRoot, StoreReader};
use crate::{
    format::{bindings::Selection, journal_stream::Summary, table::MAX_RECORD_BYTES},
    frame_changes::{Cell, CompleteChanges},
    ports::Crypto,
};

pub struct HistoryChangesInput<'r, 'c, 'm, 'b, C: Crypto> {
    stream: ChangesInput<'c, 'b, C, StoreReader<'r>>,
    crypto: &'c C,
    selection: Selection<'m>,
    index: usize,
}
impl LockedRoot {
    /// Caller admits bytes/work and retains actual recovery/view barriers and pins.
    pub fn open_history_changes<'r, 'c, 'm, 'b, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'m>,
        index: usize,
        max_bytes: u64,
        cells: &'b mut [Cell],
    ) -> Result<HistoryChangesInput<'r, 'c, 'm, 'b, C>, HistoryInputError> {
        let (file, expected) = super::history::open_selected(self, selection, index, max_bytes)?;
        let stream = ChangesInput::new(crypto, file, expected, cells)?;
        Ok(HistoryChangesInput {
            stream,
            crypto,
            selection,
            index,
        })
    }
}
impl<'r, 'b, C: Crypto> HistoryChangesInput<'r, '_, '_, 'b, C> {
    #[cfg(test)]
    pub(super) fn position(&self) -> u64 {
        self.stream.position()
    }
    pub fn is_failed(&self) -> bool {
        self.stream.is_failed()
    }
    /// Locally checked changes only; whole-file selection and final-view proof remain pending.
    pub fn frame(&self) -> Option<&CompleteChanges<'_>> {
        self.stream.frame()
    }
    /// Discards the previous frame, then reads one bounded frame. False is not EOF proof.
    pub fn advance_frame(
        &mut self,
        scratch: &mut [u8; MAX_RECORD_BYTES],
    ) -> Result<bool, HistoryInputError> {
        self.stream.advance_frame(scratch)
    }
    pub fn finish(self) -> Result<CompleteHistoryChanges<'r>, HistoryInputError> {
        let (complete, _) = self.finish_reuse()?;
        Ok(complete)
    }
    /// Checked completion plus the original full scratch capacity for another segment.
    pub fn finish_reuse(
        self,
    ) -> Result<(CompleteHistoryChanges<'r>, &'b mut [Cell]), HistoryInputError> {
        let (file, summary, cells) = self.stream.finish_reuse()?;
        self.selection
            .check_history_journal(self.crypto, self.index, summary)?;
        Ok((CompleteHistoryChanges { file, summary }, cells))
    }
}

#[derive(Debug)]
pub struct CompleteHistoryChanges<'r> {
    file: CompleteFile<'r>,
    summary: Summary,
}
impl CompleteHistoryChanges<'_> {
    pub fn file(&self) -> &CompleteFile<'_> {
        &self.file
    }
    pub fn summary(&self) -> Summary {
        self.summary
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) fn probe(
    root: &LockedRoot,
    selection: Selection<'_>,
    scratch: &mut [u8; MAX_RECORD_BYTES],
) {
    let mut cells = [Cell::EMPTY; 1];
    let mut input = root
        .open_history_changes(&td_crypto::Provider, selection, 0, 277, &mut cells)
        .unwrap();
    assert!(input.frame().is_none());
    assert!(input.advance_frame(scratch).unwrap());
    assert!(input.frame().unwrap().is_empty());
    assert_eq!(
        input.frame().unwrap().summary().header().sequence.number(),
        1
    );
    assert!(!input.advance_frame(scratch).unwrap());
    assert!(input.frame().is_none());
    let complete = input.finish().unwrap();
    assert_eq!(complete.file().len(), 277);
    assert_eq!(complete.summary().through().number(), 1);
    drop(complete);
    assert!(root
        .open_history_changes(&td_crypto::Provider, selection, 0, 276, &mut cells)
        .is_err());
    let input = root
        .open_history_changes(&td_crypto::Provider, selection, 0, 277, &mut cells)
        .unwrap();
    assert!(input.finish().is_err());
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::super::change_input::MAX_FRAME_READS;
    use super::super::tests::Fixture;
    use super::*;
    use crate::format::{
        frame::DecodeError, journal_stream::Error as StreamError, Error as FormatError,
        FRAME_HEADER_BYTES, JOURNAL_HEADER_BYTES, MAX_FRAME_OPERATIONS, OPERATION_HEADER_BYTES,
    };
    use crate::{
        format::{
            container::{Current, Error as ContainerError, JournalHeader},
            frame, manifest,
            operation::Operation,
            ObjectType, Sequence, Table, TABLE_COUNT,
        },
        ids::AccountId,
        ports::ChangeAction,
        store_paths::{AccountEntry, Name, Number},
    };
    use std::{fs, io, os::unix::fs::PermissionsExt};
    use td_crypto::{Digest, Provider};
    const ACCOUNT: AccountId = AccountId::from_bytes([0x33; 16]);
    struct Selected {
        format: Vec<u8>,
        current: Vec<u8>,
        manifest: Vec<u8>,
    }
    impl Selected {
        fn selection(&self) -> Selection<'_> {
            Selection::decode(
                &Provider,
                ACCOUNT,
                &self.format,
                &self.current,
                &self.manifest,
            )
            .unwrap()
        }
    }
    fn hex(text: &str) -> Vec<u8> {
        let text: String = text.split_whitespace().collect();
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    fn hash(bytes: &[u8]) -> [u8; 32] {
        let mut digest = Provider.sha256().unwrap();
        digest.update(bytes).unwrap();
        digest.finish().unwrap()
    }
    fn write(root: &LockedRoot, bytes: &[u8]) {
        let name = Name::account(ACCOUNT, AccountEntry::Journal(Number::new(1).unwrap())).unwrap();
        let mut buffer = [0; super::super::MAX_PATH_BYTES];
        let path = root.root.directory.join(&name, &mut buffer).unwrap();
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn setup(fixture: &Fixture, counts: &[usize]) -> (LockedRoot, Selected, Vec<u8>) {
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Metadata,
            AccountEntry::Journals,
        ] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
        let mut selected = Selected {
            format: hex(include_str!("../../tests/fixtures/format-v1/format.hex")),
            current: hex(include_str!(
                "../../tests/fixtures/format-v1/current-history.hex"
            )),
            manifest: hex(include_str!(
                "../../tests/fixtures/format-v1/manifest-history.hex"
            )),
        };
        let original = selected.selection();
        let manifest = original.manifest();
        let mut journal = vec![0; JOURNAL_HEADER_BYTES];
        JournalHeader {
            account: ACCOUNT,
            epoch: original.store().epoch,
            segment: 1,
            base: Sequence::default(),
        }
        .encode(&Provider, &mut journal)
        .unwrap();
        for (index, count) in counts.iter().copied().enumerate() {
            let mut bytes = vec![0; 104 + 28 * count];
            for (ordinal, output) in bytes[64..64 + 28 * count]
                .as_chunks_mut::<28>()
                .0
                .iter_mut()
                .enumerate()
            {
                Operation::change(
                    ObjectType::Email,
                    ChangeAction::Updated,
                    &[ordinal as u8; 16],
                )
                .encode(output)
                .unwrap();
            }
            frame::seal(
                &Provider,
                Sequence::from_u64(index as u64 + 1),
                count,
                &mut bytes,
            )
            .unwrap();
            journal.extend_from_slice(&bytes);
        }
        let through = Sequence::from_u64(counts.len() as u64);
        let header = manifest::Header {
            through,
            ..manifest.header()
        };
        let tables = std::array::from_fn::<_, TABLE_COUNT, _>(|i| {
            manifest
                .table(Table::from_tag(i as u16 + 1).unwrap())
                .unwrap()
        });
        let history = manifest::HistoryDescriptor {
            through,
            file_bytes: journal.len() as u64,
            digest: hash(&journal),
            ..manifest.history(0).unwrap()
        };
        let current = original.current();
        let mut bytes = vec![0; crate::format::MAX_MANIFEST_BYTES];
        let n = manifest::encode(&Provider, header, &tables, &[history], &mut bytes).unwrap();
        bytes.truncate(n);
        selected.manifest = bytes;
        Current {
            manifest_digest: hash(&selected.manifest),
            ..current
        }
        .encode(&Provider, &mut selected.current)
        .unwrap();
        write(&root, &journal);
        (root, selected, journal)
    }
    #[test]
    fn frames_reuse_full_slots_and_finish_against_selected_history() {
        let fixture = Fixture::new();
        let (root, selected, bytes) = setup(&fixture, &[4096, 1, 3]);
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        let mut cells = vec![Cell::EMPTY; MAX_FRAME_OPERATIONS];
        let mut input = root
            .open_history_changes(
                &Provider,
                selected.selection(),
                0,
                bytes.len() as u64,
                &mut cells,
            )
            .unwrap();
        for (index, count) in [4096, 1, 3].into_iter().enumerate() {
            assert!(input
                .advance_frame(scratch.as_mut_slice().try_into().unwrap())
                .unwrap());
            let frame = input.frame().unwrap();
            scratch.fill(0xa5); // Reusable by row lookups while completed changes remain borrowed.
            assert_eq!(frame.len(), count);
            for (ordinal, record) in frame.records().enumerate() {
                assert_eq!(record.cursor.sequence.number(), index as u64 + 1);
                assert_eq!(record.cursor.operation as usize, ordinal);
                assert_eq!(record.change.id, [ordinal as u8; 16]);
            }
        }
        assert!(!input
            .advance_frame(scratch.as_mut_slice().try_into().unwrap())
            .unwrap());
        assert!(input.frame().is_none());
        let complete = input.finish().unwrap();
        assert_eq!(complete.file().len(), bytes.len() as u64);
        assert_eq!(complete.summary().operations(), 4100);
        assert_eq!(complete.summary().digest(), hash(&bytes));
        assert!(std::mem::size_of::<HistoryChangesInput<'_, '_, '_, '_, Provider>>() <= 8192);
    }
    #[test]
    fn io_capacity_and_frame_errors_retire_and_clear_previous_results() {
        for mode in 0..4 {
            let fixture = Fixture::new();
            let (root, selected, mut bytes) = setup(&fixture, &[1, 2]);
            let mut scratch = vec![0; MAX_RECORD_BYTES];
            let mut cells = [Cell::EMPTY; 2];
            let slots = if mode == 0 { 1 } else { 2 };
            let mut input = root
                .open_history_changes(
                    &Provider,
                    selected.selection(),
                    0,
                    bytes.len() as u64,
                    &mut cells[..slots],
                )
                .unwrap();
            assert!(input
                .advance_frame(scratch.as_mut_slice().try_into().unwrap())
                .unwrap());
            if mode == 1 {
                bytes.pop();
                write(&root, &bytes);
            }
            if mode == 2 {
                *bytes.last_mut().unwrap() ^= 1;
                write(&root, &bytes);
            }
            let error = if mode == 3 {
                input
                    .stream
                    .advance_using(scratch.as_mut_slice().try_into().unwrap(), |_, _| {
                        Err(io::ErrorKind::Interrupted.into())
                    })
            } else {
                input.advance_frame(scratch.as_mut_slice().try_into().unwrap())
            };
            match (mode, error) {
                (
                    0,
                    Err(HistoryInputError::Stream(StreamError::Journal(ContainerError::Format(
                        FormatError::OutputFull,
                    )))),
                ) => {}
                (1, Err(HistoryInputError::Io(e))) => {
                    assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof)
                }
                (
                    2,
                    Err(HistoryInputError::Stream(StreamError::Frame(DecodeError::Invalid(
                        ContainerError::Checksum,
                    )))),
                ) => {}
                (3, Err(HistoryInputError::Io(e))) => {
                    assert_eq!(e.kind(), io::ErrorKind::Interrupted)
                }
                (_, result) => panic!("mode {mode}: {result:?}"),
            }
            assert!(input.is_failed());
            assert!(input.frame().is_none());
            assert!(
                matches!(input.advance_frame(scratch.as_mut_slice().try_into().unwrap()), Err(HistoryInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
            );
            assert!(
                matches!(input.finish(), Err(HistoryInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
            );
        }
    }
    #[test]
    fn opening_refuses_unselected_extent_and_header_before_frame_input() {
        for mode in 0..6 {
            let fixture = Fixture::new();
            let (root, selected, mut bytes) = setup(&fixture, &[1]);
            let admitted = bytes.len() as u64;
            match mode {
                2 => {
                    bytes.pop();
                }
                3 => bytes.push(0),
                4 => bytes[0] ^= 1,
                5 => {
                    let mut header =
                        JournalHeader::decode(&Provider, &bytes[..JOURNAL_HEADER_BYTES]).unwrap();
                    header.segment = 2;
                    header
                        .encode(&Provider, &mut bytes[..JOURNAL_HEADER_BYTES])
                        .unwrap();
                }
                _ => {}
            }
            write(&root, &bytes);
            let mut cells = [Cell::EMPTY; 1];
            let result = root.open_history_changes(
                &Provider,
                selected.selection(),
                usize::from(mode == 0),
                admitted - u64::from(mode == 1),
                &mut cells,
            );
            match (mode, result) {
                (
                    0,
                    Err(HistoryInputError::Stream(StreamError::Journal(ContainerError::Format(
                        FormatError::InvalidValue,
                    )))),
                ) => {}
                (1, Err(HistoryInputError::Io(e))) => {
                    assert_eq!(e.kind(), io::ErrorKind::InvalidInput)
                }
                (2 | 3, Err(HistoryInputError::Io(e))) => {
                    assert_eq!(e.kind(), io::ErrorKind::InvalidData)
                }
                (
                    4,
                    Err(HistoryInputError::Stream(StreamError::Journal(ContainerError::Checksum))),
                ) => {}
                (
                    5,
                    Err(HistoryInputError::Stream(StreamError::Journal(ContainerError::Format(
                        FormatError::InvalidValue,
                    )))),
                ) => {}
                _ => panic!("unexpected result for mode {mode}"),
            }
        }
    }
    #[test]
    fn declared_frame_and_operation_lengths_stop_before_excess_reads() {
        for mode in 0..2 {
            let fixture = Fixture::new();
            let (root, selected, mut bytes) = setup(&fixture, &[1]);
            if mode == 0 {
                let span =
                    &mut bytes[JOURNAL_HEADER_BYTES..JOURNAL_HEADER_BYTES + FRAME_HEADER_BYTES];
                let mut header =
                    crate::format::frame_header::Header::decode(&Provider, span).unwrap();
                header.frame_bytes += 1;
                header.encode(&Provider, span).unwrap();
            } else {
                let prefix = &mut bytes[JOURNAL_HEADER_BYTES + FRAME_HEADER_BYTES..]
                    [..OPERATION_HEADER_BYTES];
                prefix.copy_from_slice(&[2, 0, 1, 0, 17, 0, 0, 0, 0, 0, 0, 0]);
            }
            write(&root, &bytes);
            let mut scratch = vec![0; MAX_RECORD_BYTES];
            let mut cells = [Cell::EMPTY; 1];
            let mut input = root
                .open_history_changes(
                    &Provider,
                    selected.selection(),
                    0,
                    bytes.len() as u64,
                    &mut cells,
                )
                .unwrap();
            let mut reads = 0;
            assert!(matches!(
                input.stream.advance_using(
                    scratch.as_mut_slice().try_into().unwrap(),
                    |file, output| {
                        reads += 1;
                        file.read(output)
                    }
                ),
                Err(HistoryInputError::Stream(StreamError::Frame(
                    DecodeError::Invalid(crate::format::container::Error::Format(
                        FormatError::InvalidValue
                    ))
                )))
            ));
            assert_eq!(reads, mode + 1);
            assert_eq!(
                input.stream.position(),
                (JOURNAL_HEADER_BYTES + FRAME_HEADER_BYTES + mode * OPERATION_HEADER_BYTES) as u64
            );
            assert!(input.is_failed());
            assert!(input.frame().is_none());
            assert!(input.finish().is_err());
        }
    }
    #[test]
    fn read_attempt_budget_and_late_selected_digest_cannot_be_skipped() {
        let fixture = Fixture::new();
        let (root, selected, mut bytes) = setup(&fixture, &[4096]);
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        let mut cells = vec![Cell::EMPTY; MAX_FRAME_OPERATIONS];
        let mut input = root
            .open_history_changes(
                &Provider,
                selected.selection(),
                0,
                bytes.len() as u64,
                &mut cells,
            )
            .unwrap();
        let mut calls = 0;
        assert!(
            matches!(input.stream.advance_using(scratch.as_mut_slice().try_into().unwrap(), |file, output| { calls+=1; file.read(output.get_mut(..1).unwrap()) }),
            Err(HistoryInputError::Io(e)) if e.kind()==io::ErrorKind::WouldBlock)
        );
        assert_eq!(calls, MAX_FRAME_READS);
        assert!(input.finish().is_err());
        bytes[JOURNAL_HEADER_BYTES + FRAME_HEADER_BYTES + OPERATION_HEADER_BYTES] ^= 1;
        frame::seal(
            &Provider,
            Sequence::from_u64(1),
            4096,
            &mut bytes[JOURNAL_HEADER_BYTES..],
        )
        .unwrap();
        write(&root, &bytes);
        let mut input = root
            .open_history_changes(
                &Provider,
                selected.selection(),
                0,
                bytes.len() as u64,
                &mut cells,
            )
            .unwrap();
        assert!(input
            .advance_frame(scratch.as_mut_slice().try_into().unwrap())
            .unwrap());
        assert!(matches!(
            input.finish(),
            Err(HistoryInputError::Stream(StreamError::Journal(
                crate::format::container::Error::Checksum
            )))
        ));
    }
}
