//! Retained-history streams; active prefix reads and incomplete-tail repair are separate.
use super::{
    input::{fill_exact, fill_exact_using},
    CompleteFile, LockedRoot, StoreReader,
};
use crate::{
    format::{
        bindings::Selection,
        container::Error as ContainerError,
        frame::Frame,
        frame_header::Header as FrameHeader,
        journal_stream::{Error as StreamError, Summary, Verifier},
        Error as FormatError, Sequence, FRAME_HEADER_BYTES, JOURNAL_HEADER_BYTES, MAX_FRAME_BYTES,
        MAX_JOURNAL_FRAME_BYTES,
    },
    ports::Crypto,
    store_paths::{AccountEntry, Number},
};
use std::io;
const MAX_HISTORY_READ_CALLS: usize = 64;
#[derive(Debug)]
pub enum HistoryInputError {
    Io(io::Error),
    Stream(StreamError),
}
impl From<io::Error> for HistoryInputError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<StreamError> for HistoryInputError {
    fn from(e: StreamError) -> Self {
        Self::Stream(e)
    }
}
impl From<ContainerError> for HistoryInputError {
    fn from(e: ContainerError) -> Self {
        Self::Stream(e.into())
    }
}
impl From<FormatError> for HistoryInputError {
    fn from(e: FormatError) -> Self {
        Self::Stream(e.into())
    }
}
impl std::fmt::Display for HistoryInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "history input I/O: {e}"),
            Self::Stream(e) => write!(f, "history input validation: {e}"),
        }
    }
}
impl std::error::Error for HistoryInputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Stream(e) => Some(e),
        }
    }
}
pub struct HistoryInput<'r, 'c, 'm, 'b, C: Crypto> {
    file: StoreReader<'r>,
    verifier: Verifier<'c, C>,
    crypto: &'c C,
    selection: Selection<'m>,
    index: usize,
    through: Sequence,
    scratch: &'b mut [u8; MAX_FRAME_BYTES],
    failed: bool,
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
        let descriptor = selection.manifest().history(index)?;
        if descriptor.file_bytes > (JOURNAL_HEADER_BYTES + MAX_JOURNAL_FRAME_BYTES) as u64 {
            return Err(FormatError::Limit.into());
        }
        if descriptor.file_bytes > max_bytes {
            return Err(io::Error::from(io::ErrorKind::InvalidInput).into());
        }
        let segment = Number::new(descriptor.segment).map_err(|_| FormatError::InvalidValue)?;
        let mut file = self.open_account_file(
            selection.current().account,
            AccountEntry::Journal(segment),
            descriptor.file_bytes,
        )?;
        if file.len() != descriptor.file_bytes {
            return Err(io::Error::from(io::ErrorKind::InvalidData).into());
        }
        let mut bytes = [0; JOURNAL_HEADER_BYTES];
        let mut attempts = MAX_HISTORY_READ_CALLS;
        fill_exact(&mut file, &mut bytes, &mut attempts)?;
        let verifier = Verifier::new(crypto, &bytes)?;
        let header = verifier.header();
        if header.account != selection.current().account
            || header.epoch != selection.store().epoch
            || header.segment != descriptor.segment
            || header.base != descriptor.base
        {
            return Err(FormatError::InvalidValue.into());
        }
        Ok(HistoryInput {
            file,
            verifier,
            crypto,
            selection,
            index,
            through: header.base,
            scratch,
            failed: false,
        })
    }
}
impl<'r, C: Crypto> HistoryInput<'r, '_, '_, '_, C> {
    pub fn is_failed(&self) -> bool {
        self.failed
    }
    /// One provisional frame; None is not physical EOF or selected completion.
    pub fn next_frame(&mut self) -> Result<Option<Frame<'_>>, HistoryInputError> {
        self.next_frame_using(StoreReader::read)
    }
    fn next_frame_using(
        &mut self,
        mut read: impl FnMut(&mut StoreReader<'r>, &mut [u8]) -> io::Result<usize>,
    ) -> Result<Option<Frame<'_>>, HistoryInputError> {
        if self.failed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe).into());
        }
        if self.file.position() == self.file.len() {
            return Ok(None);
        }
        self.failed = true;
        let remaining = self
            .file
            .len()
            .checked_sub(self.file.position())
            .ok_or(FormatError::InvalidValue)?;
        if remaining < FRAME_HEADER_BYTES as u64 {
            return Err(StreamError::Frame(FormatError::InvalidValue.into()).into());
        }
        let prefix = self
            .scratch
            .get_mut(..FRAME_HEADER_BYTES)
            .ok_or(FormatError::Limit)?;
        let mut attempts = MAX_HISTORY_READ_CALLS;
        fill_exact_using(&mut self.file, prefix, &mut attempts, &mut read)?;
        let header =
            FrameHeader::decode(self.crypto, prefix).map_err(|e| StreamError::Frame(e.into()))?;
        if header.sequence != self.through.successor()? {
            return Err(StreamError::Frame(FormatError::InvalidValue.into()).into());
        }
        if header.frame_bytes as u64 > remaining {
            return Err(StreamError::Frame(FormatError::InvalidValue.into()).into());
        }
        let bytes = self
            .scratch
            .get_mut(..header.frame_bytes)
            .ok_or(FormatError::Limit)?;
        fill_exact_using(
            &mut self.file,
            bytes
                .get_mut(FRAME_HEADER_BYTES..)
                .ok_or(FormatError::Limit)?,
            &mut attempts,
            &mut read,
        )?;
        let frame = self.verifier.push(bytes)?;
        self.through = frame.header().sequence;
        self.failed = false;
        Ok(Some(frame))
    }
}
impl<'r, C: Crypto> HistoryInput<'r, '_, '_, '_, C> {
    pub fn finish(self) -> Result<CompleteHistory<'r>, HistoryInputError> {
        if self.failed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe).into());
        }
        let summary = self.verifier.finish()?;
        let file = self.file.finish()?;
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
            assert_eq!(input.file.position(), frame_end);
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
            assert_eq!(input.file.position(), end as u64);
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
            input.file.position(),
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
