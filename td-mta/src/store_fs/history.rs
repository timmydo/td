//! Retained-history streams; active prefix reads and incomplete-tail repair are separate.
pub use super::journal_input::Error as HistoryInputError;
#[cfg(test)]
use super::journal_input::MAX_READ_CALLS as MAX_HISTORY_READ_CALLS;
use super::{journal_input::FrameInput, CompleteFile, LockedRoot, StoreReader};
#[cfg(test)]
use crate::format::{
    container::Error as ContainerError, frame_header::Header as FrameHeader,
    journal_stream::Error as StreamError, Sequence, FRAME_HEADER_BYTES,
};
use crate::{
    format::{
        bindings::Selection, container::JournalHeader, frame::Frame, journal_stream::Summary,
        Error as FormatError, JOURNAL_HEADER_BYTES, MAX_FRAME_BYTES, MAX_JOURNAL_FRAME_BYTES,
    },
    ports::Crypto,
    store_paths::{AccountEntry, Number},
};
use std::io;

#[path = "history/sweep.rs"]
mod sweep;
#[cfg(test)]
pub(super) use sweep::probe as probe_sweep;
pub use sweep::{
    CompleteHistorySweep, HistorySweep, HistorySweepError, HistorySweepLimits, HistorySweepStep,
};

pub struct HistoryInput<'r, 'c, 'm, 'b, C: Crypto> {
    stream: FrameInput<'c, 'b, C, StoreReader<'r>>,
    crypto: &'c C,
    selection: Selection<'m>,
    index: usize,
}
impl LockedRoot {
    /// Caller holds a real recovery/view barrier and admits the read's bytes/work.
    pub fn open_history<'r, 'c, 'm, 'b, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'m>,
        index: usize,
        max_bytes: u64,
        scratch: &'b mut [u8; MAX_FRAME_BYTES],
    ) -> Result<HistoryInput<'r, 'c, 'm, 'b, C>, HistoryInputError> {
        let (file, expected) = open_selected(self, selection, index, max_bytes)?;
        let stream = FrameInput::new(crypto, file, expected, scratch)?;
        Ok(HistoryInput {
            stream,
            crypto,
            selection,
            index,
        })
    }
}
pub(super) fn open_selected<'r>(
    root: &'r LockedRoot,
    selection: Selection<'_>,
    index: usize,
    max_bytes: u64,
) -> Result<(StoreReader<'r>, JournalHeader), HistoryInputError> {
    let descriptor = selection.manifest().history(index)?;
    if descriptor.file_bytes > (JOURNAL_HEADER_BYTES + MAX_JOURNAL_FRAME_BYTES) as u64 {
        return Err(FormatError::Limit.into());
    }
    if descriptor.file_bytes > max_bytes {
        return Err(io::Error::from(io::ErrorKind::InvalidInput).into());
    }
    let segment = Number::new(descriptor.segment).map_err(|_| FormatError::InvalidValue)?;
    let file = root.open_account_file(
        selection.current().account,
        AccountEntry::Journal(segment),
        descriptor.file_bytes,
    )?;
    if file.len() != descriptor.file_bytes {
        return Err(io::Error::from(io::ErrorKind::InvalidData).into());
    }
    let expected = JournalHeader {
        account: selection.current().account,
        epoch: selection.store().epoch,
        segment: descriptor.segment,
        base: descriptor.base,
    };
    Ok((file, expected))
}
impl<'r, C: Crypto> HistoryInput<'r, '_, '_, '_, C> {
    pub fn is_failed(&self) -> bool {
        self.stream.is_failed()
    }
    /// One provisional frame; None is not physical EOF or selected completion.
    pub fn next_frame(&mut self) -> Result<Option<Frame<'_>>, HistoryInputError> {
        self.stream.next_frame()
    }
    #[cfg(test)]
    fn next_frame_using(
        &mut self,
        read: impl FnMut(&mut StoreReader<'r>, &mut [u8]) -> io::Result<usize>,
    ) -> Result<Option<Frame<'_>>, HistoryInputError> {
        self.stream.next_frame_using(read)
    }
    pub fn finish(self) -> Result<CompleteHistory<'r>, HistoryInputError> {
        let (file, summary) = self.stream.finish()?;
        self.selection
            .check_history_journal(self.crypto, self.index, summary)?;
        Ok(CompleteHistory { file, summary })
    }
}
#[derive(Debug)]
pub struct CompleteHistory<'a> {
    file: CompleteFile<'a>,
    summary: Summary,
}
impl CompleteHistory<'_> {
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
    pub(super) const JOURNAL: &str =
        include_str!("../../tests/fixtures/format-v1/journal-with-frame.hex");
    pub(super) fn hex(text: &str) -> Vec<u8> {
        let text: String = text.split_whitespace().collect();
        let (pairs, rest) = text.as_bytes().as_chunks::<2>();
        assert!(rest.is_empty());
        pairs
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    pub(super) fn name() -> Name {
        Name::account(ACCOUNT, AccountEntry::Journal(Number::new(1).unwrap())).unwrap()
    }
    pub(super) fn write(root: &LockedRoot, bytes: &[u8]) {
        let mut buffer = [0; super::super::MAX_PATH_BYTES];
        let path = root.root.directory.join(&name(), &mut buffer).unwrap();
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
        root.create_account_directory(ACCOUNT, AccountEntry::Journals)
            .unwrap();
        write(root, &hex(JOURNAL));
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
pub(super) fn probe(root: &LockedRoot, bytes: &ProbeBytes, scratch: &mut [u8; MAX_FRAME_BYTES]) {
    let selection = bytes.selection();
    let mut input = root
        .open_history(&td_crypto::Provider, selection, 0, 277, scratch)
        .unwrap();
    let frame = input.next_frame().unwrap().unwrap();
    assert_eq!(frame.header().sequence.number(), 1);
    let mut operations = frame.operations();
    assert!(operations.next().unwrap().is_ok());
    assert!(operations.next().is_none());
    assert!(input.next_frame().unwrap().is_none());
    let complete = input.finish().unwrap();
    assert_eq!(complete.file().len(), 277);
    assert_eq!(complete.summary().through().number(), 1);
    assert_eq!(complete.summary().operations(), 1);
    drop(complete);
    assert!(matches!(
        root.open_history(&td_crypto::Provider, selection, 1, 277, scratch),
        Err(HistoryInputError::Stream(_))
    ));
    assert!(
        matches!(root.open_history(&td_crypto::Provider, selection, 0, 276, scratch), Err(HistoryInputError::Io(e)) if e.kind() == io::ErrorKind::InvalidInput)
    );
    let input = root
        .open_history(&td_crypto::Provider, selection, 0, 277, scratch)
        .unwrap();
    assert!(
        matches!(input.finish(),Err(HistoryInputError::Io(e)) if e.kind() == io::ErrorKind::InvalidInput)
    );
    super::history_changes::probe(
        root,
        selection,
        scratch
            .get_mut(..crate::format::table::MAX_RECORD_BYTES)
            .unwrap()
            .try_into()
            .unwrap(),
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::tests::Fixture;
    use super::{fixture::*, *};
    use crate::{
        format::{
            container::{Current, JournalHeader},
            frame::DecodeError,
            manifest, Table, TABLE_COUNT,
        },
        ids::{AccountId, StoreEpoch},
    };
    use td_crypto::{Digest, Provider};
    fn setup(fixture: &Fixture) -> (LockedRoot, Bytes) {
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        root.create_account_directory(ACCOUNT, AccountEntry::Root)
            .unwrap();
        root.create_account_directory(ACCOUNT, AccountEntry::Metadata)
            .unwrap();
        let bytes = prepare(&root);
        (root, bytes)
    }
    fn digest(bytes: &[u8]) -> [u8; 32] {
        let mut hash = Provider.sha256().unwrap();
        hash.update(bytes).unwrap();
        hash.finish().unwrap()
    }
    fn rehash(bytes: &mut [u8]) {
        let end = bytes.len() - 32;
        let hash = digest(bytes.get(..end).unwrap());
        bytes.get_mut(end..).unwrap().copy_from_slice(&hash);
    }
    fn scratch(buffer: &mut [u8]) -> &mut [u8; MAX_FRAME_BYTES] {
        buffer.try_into().unwrap()
    }
    fn two_frames(root: &LockedRoot, bytes: &mut Bytes) -> Vec<u8> {
        let mut journal = hex(JOURNAL);
        journal.extend_from_slice(&hex(include_str!(
            "../../tests/fixtures/format-v1/frame-delete-change.hex"
        )));
        select_history(bytes, 2, journal.len() as u64, digest(&journal));
        write(root, &journal);
        journal
    }
    fn select_history(bytes: &mut Bytes, through: u64, file_bytes: u64, hash: [u8; 32]) {
        let selection = bytes.selection();
        let manifest = selection.manifest();
        let tables: [manifest::TableDescriptor; TABLE_COUNT] = std::array::from_fn(|index| {
            manifest
                .table(Table::from_tag((index + 1) as u16).unwrap())
                .unwrap()
        });
        let history = manifest::HistoryDescriptor {
            through: Sequence::from_u64(through),
            file_bytes,
            digest: hash,
            ..manifest.history(0).unwrap()
        };
        let header = manifest::Header {
            through: Sequence::from_u64(through),
            ..manifest.header()
        };
        let mut output = vec![0; crate::format::MAX_MANIFEST_BYTES];
        let length = manifest::encode(&Provider, header, &tables, &[history], &mut output).unwrap();
        output.truncate(length);
        bytes.manifest = output;
        let current = Current {
            manifest_digest: digest(&bytes.manifest),
            ..Current::decode(&Provider, &bytes.current).unwrap()
        };
        current.encode(&Provider, &mut bytes.current).unwrap();
    }
    #[test]
    fn selected_history_streams_literal_frames_and_reuses_scratch() {
        let fixture = Fixture::new();
        let (root, mut bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        probe(&root, &bytes, scratch(&mut buffer));
        let journal = two_frames(&root, &mut bytes);
        let mut input = root
            .open_history(
                &Provider,
                bytes.selection(),
                0,
                journal.len() as u64,
                scratch(&mut buffer),
            )
            .unwrap();
        for sequence in [1, 2] {
            assert_eq!(
                input
                    .next_frame()
                    .unwrap()
                    .unwrap()
                    .header()
                    .sequence
                    .number(),
                sequence
            );
        }
        assert!(input.next_frame().unwrap().is_none());
        let complete = input.finish().unwrap();
        assert_eq!(complete.summary().operations(), 3);
        assert_eq!(complete.summary().through().number(), 2);
        assert_eq!(complete.file().len(), journal.len() as u64);
    }
    #[test]
    fn history_open_requires_exact_extent_and_valid_selected_header() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let selection = bytes.selection();
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        for length in [0, 95, 276, 278] {
            let mut journal = hex(JOURNAL);
            journal.resize(length, 0);
            write(&root, &journal);
            assert!(
                matches!(root.open_history(&Provider,selection,0,277,scratch(&mut buffer)),Err(HistoryInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidData)
            );
        }
        let original = hex(JOURNAL);
        let header =
            JournalHeader::decode(&Provider, original.get(..JOURNAL_HEADER_BYTES).unwrap())
                .unwrap();
        for changed in [
            JournalHeader {
                account: AccountId::from_bytes([9; 16]),
                ..header
            },
            JournalHeader {
                epoch: StoreEpoch::from_bytes([9; 16]),
                ..header
            },
            JournalHeader {
                segment: 2,
                ..header
            },
            JournalHeader {
                base: Sequence::from_u64(1),
                ..header
            },
        ] {
            let mut journal = original.clone();
            changed
                .encode(&Provider, journal.get_mut(..JOURNAL_HEADER_BYTES).unwrap())
                .unwrap();
            write(&root, &journal);
            assert!(matches!(
                root.open_history(&Provider, selection, 0, 277, scratch(&mut buffer)),
                Err(HistoryInputError::Stream(StreamError::Journal(
                    ContainerError::Format(FormatError::InvalidValue)
                )))
            ));
        }
    }
    #[test]
    fn history_format_ceiling_is_distinct_from_caller_admission() {
        let fixture = Fixture::new();
        let (root, mut bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        let file_bytes = (JOURNAL_HEADER_BYTES + MAX_JOURNAL_FRAME_BYTES + 1) as u64;
        select_history(&mut bytes, 5, file_bytes, [0; 32]);
        for admitted in [0, file_bytes] {
            assert!(matches!(
                root.open_history_changes(&Provider, bytes.selection(), 0, admitted, &mut []),
                Err(HistoryInputError::Stream(StreamError::Journal(
                    ContainerError::Format(FormatError::Limit)
                )))
            ));
            assert!(matches!(
                root.open_history(
                    &Provider,
                    bytes.selection(),
                    0,
                    admitted,
                    scratch(&mut buffer)
                ),
                Err(HistoryInputError::Stream(StreamError::Journal(
                    ContainerError::Format(FormatError::Limit)
                )))
            ));
        }
    }
    #[test]
    fn history_short_recorded_suffix_is_format_corruption_before_io() {
        let fixture = Fixture::new();
        let (root, mut bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        for suffix in 1..FRAME_HEADER_BYTES {
            let mut journal = hex(JOURNAL);
            let frame_end = journal.len() as u64;
            journal.resize(journal.len() + suffix, 0);
            select_history(&mut bytes, 1, journal.len() as u64, digest(&journal));
            write(&root, &journal);
            let mut input = root
                .open_history(
                    &Provider,
                    bytes.selection(),
                    0,
                    journal.len() as u64,
                    scratch(&mut buffer),
                )
                .unwrap();
            assert!(input.next_frame().unwrap().is_some());
            let mut calls = 0;
            assert!(matches!(
                input.next_frame_using(|file, output| {
                    calls += 1;
                    file.read(output)
                }),
                Err(HistoryInputError::Stream(StreamError::Frame(
                    DecodeError::Invalid(ContainerError::Format(FormatError::InvalidValue))
                )))
            ));
            assert_eq!(calls, 0);
            assert_eq!(input.stream.position(), frame_end);
            assert!(input.is_failed());
            assert!(
                matches!(input.finish(), Err(HistoryInputError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe)
            );
        }
    }
    #[test]
    fn history_frame_header_errors_refuse_before_remainder_and_retire() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let selection = bytes.selection();
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        let original = hex(JOURNAL);
        let start = JOURNAL_HEADER_BYTES;
        let end = start + FRAME_HEADER_BYTES;
        let header = FrameHeader::decode(&Provider, original.get(start..end).unwrap()).unwrap();
        for mode in 0..4 {
            let mut journal = original.clone();
            match mode {
                0 => {
                    *journal.get_mut(end - 1).unwrap() ^= 1;
                }
                1 => {
                    FrameHeader {
                        sequence: Sequence::from_u64(2),
                        ..header
                    }
                    .encode(&Provider, journal.get_mut(start..end).unwrap())
                    .unwrap();
                }
                2 => {
                    FrameHeader {
                        frame_bytes: header.frame_bytes + 1,
                        ..header
                    }
                    .encode(&Provider, journal.get_mut(start..end).unwrap())
                    .unwrap();
                }
                _ => {
                    journal
                        .get_mut(start + 16..start + 20)
                        .unwrap()
                        .copy_from_slice(&u32::MAX.to_le_bytes());
                    rehash(journal.get_mut(start..end).unwrap());
                }
            }
            write(&root, &journal);
            let mut input = root
                .open_history(&Provider, selection, 0, 277, scratch(&mut buffer))
                .unwrap();
            assert!(matches!(
                input.next_frame(),
                Err(HistoryInputError::Stream(StreamError::Frame(
                    DecodeError::Invalid(_)
                )))
            ));
            assert_eq!(input.stream.position(), end as u64);
            assert!(input.is_failed());
            assert!(
                matches!(input.next_frame(),Err(HistoryInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
            );
            assert!(
                matches!(input.finish(),Err(HistoryInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
            );
        }
    }
    #[test]
    fn history_truncation_footer_and_read_budget_failures_cannot_resume() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let selection = bytes.selection();
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        for mode in 0..3 {
            let original = hex(JOURNAL);
            write(&root, &original);
            let mut input = root
                .open_history(&Provider, selection, 0, 277, scratch(&mut buffer))
                .unwrap();
            let mut changed = original.clone();
            match mode {
                0 => changed.truncate(96),
                1 => changed.truncate(170),
                _ => *changed.last_mut().unwrap() ^= 1,
            }
            write(&root, &changed);
            let error = input.next_frame();
            if mode < 2 {
                assert!(
                    matches!(error, Err(HistoryInputError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof)
                );
            } else {
                assert!(matches!(
                    error,
                    Err(HistoryInputError::Stream(StreamError::Frame(
                        DecodeError::Invalid(ContainerError::Checksum)
                    )))
                ));
            }
            assert!(input.is_failed());
            assert!(
                matches!(input.finish(),Err(HistoryInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
            );
        }
        write(&root, &hex(JOURNAL));
        let mut input = root
            .open_history(&Provider, selection, 0, 277, scratch(&mut buffer))
            .unwrap();
        let mut calls = 0;
        assert!(
            matches!(input.next_frame_using(|file,output| {calls+=1;file.read(output.get_mut(..1).unwrap())}),Err(HistoryInputError::Io(e)) if e.kind()==io::ErrorKind::WouldBlock)
        );
        assert_eq!(calls, MAX_HISTORY_READ_CALLS);
        assert_eq!(
            input.stream.position(),
            (JOURNAL_HEADER_BYTES + FRAME_HEADER_BYTES) as u64
        );
        assert!(input.is_failed());
        assert!(
            matches!(input.next_frame(),Err(HistoryInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
        );
        assert!(
            matches!(input.finish(),Err(HistoryInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
        );
    }
    #[test]
    fn history_completion_requires_physical_eof_and_selected_digest() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let selection = bytes.selection();
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        for length in [0, 278] {
            write(&root, &hex(JOURNAL));
            let mut input = root
                .open_history(&Provider, selection, 0, 277, scratch(&mut buffer))
                .unwrap();
            assert!(input.next_frame().unwrap().is_some());
            let mut changed = hex(JOURNAL);
            changed.resize(length, 0);
            write(&root, &changed);
            assert!(
                matches!(input.finish(),Err(HistoryInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidData)
            );
        }
        let mut changed = hex(JOURNAL);
        *changed
            .get_mut(
                JOURNAL_HEADER_BYTES + FRAME_HEADER_BYTES + crate::format::OPERATION_HEADER_BYTES,
            )
            .unwrap() ^= 1;
        rehash(changed.get_mut(JOURNAL_HEADER_BYTES..).unwrap());
        write(&root, &changed);
        let mut input = root
            .open_history(&Provider, selection, 0, 277, scratch(&mut buffer))
            .unwrap();
        assert!(input.next_frame().unwrap().is_some());
        assert!(input.next_frame().unwrap().is_none());
        assert!(matches!(
            input.finish(),
            Err(HistoryInputError::Stream(StreamError::Journal(
                ContainerError::Checksum
            )))
        ));
    }
}
