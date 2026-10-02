//! Stopped-journal scanning with explicit consuming repair and append transitions.
pub use super::super::journal_input::Error as RecoveryInputError;
use super::super::{journal_input::MAX_READ_CALLS, LockedRoot};
use super::{fill_exact, fill_exact_using, CompleteFile, StoreReader};
use crate::{
    format::{
        bindings::Selection,
        container::{Current, JournalHeader},
        frame::Frame,
        frame_header::Header as FrameHeader,
        journal_stream::{Error as StreamError, Summary, Verifier},
        Error as FormatError, Sequence, FRAME_HEADER_BYTES, JOURNAL_HEADER_BYTES, MAX_FRAME_BYTES,
        MAX_JOURNAL_FRAME_BYTES, MAX_JOURNAL_OPERATIONS, MIN_FRAME_BYTES,
    },
    ports::Crypto,
    store_paths::{AccountEntry, Number},
};
use std::io;

#[path = "recovery/append.rs"]
mod append;
#[cfg(test)]
pub use append::probe_reserved_append;
pub use append::{
    AppendError, AppendStep, JournalAppend, ReconciledAppend, ReservedAppend, ReservedAppendError,
    SyncedAppend,
};

#[path = "recovery/repair.rs"]
mod repair;
#[cfg(test)]
pub(crate) use repair::{prepare_probe as prepare_repair_probe, probe as probe_repair};
pub use repair::{RepairError, RepairedJournal};

pub struct RecoveryInput<'r, 'c, 'b, C: Crypto> {
    file: StoreReader<'r>,
    verifier: Verifier<'c, C>,
    crypto: &'c C,
    through: Sequence,
    operations: usize,
    current: Current,
    scratch: &'b mut [u8; MAX_FRAME_BYTES],
    failed: bool,
}
impl LockedRoot {
    /// Caller holds actual stopped-store recovery exclusion; no live reader or writer.
    pub fn scan_active_journal<'r, 'c, 'b, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'_>,
        max_bytes: u64,
        scratch: &'b mut [u8; MAX_FRAME_BYTES],
    ) -> Result<RecoveryInput<'r, 'c, 'b, C>, RecoveryInputError> {
        if max_bytes < JOURNAL_HEADER_BYTES as u64 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput).into());
        }
        let selected = selection.manifest().header();
        let segment =
            Number::new(selected.active_segment).map_err(|_| FormatError::InvalidValue)?;
        let mut file = self.open_account_file(
            selected.account,
            AccountEntry::Journal(segment),
            max_bytes.min((JOURNAL_HEADER_BYTES + MAX_JOURNAL_FRAME_BYTES) as u64),
        )?;
        if file.len() < JOURNAL_HEADER_BYTES as u64 {
            return Err(FormatError::Truncated.into());
        }
        let mut header_bytes = [0; JOURNAL_HEADER_BYTES];
        let mut attempts = MAX_READ_CALLS;
        fill_exact(&mut file, &mut header_bytes, &mut attempts)?;
        let verifier = Verifier::new(crypto, &header_bytes)?;
        let expected = JournalHeader {
            account: selected.account,
            epoch: selected.epoch,
            segment: selected.active_segment,
            base: selected.through,
        };
        if verifier.header() != expected {
            return Err(FormatError::InvalidValue.into());
        }
        Ok(RecoveryInput {
            file,
            verifier,
            crypto,
            through: selected.through,
            operations: 0,
            current: selection.current(),
            scratch,
            failed: false,
        })
    }
}
impl<'r, C: Crypto> RecoveryInput<'r, '_, '_, C> {
    pub fn is_failed(&self) -> bool {
        self.failed
    }
    /// Frames are provisional; None still requires finish's physical EOF validation.
    pub fn next_frame(&mut self) -> Result<Option<Frame<'_>>, RecoveryInputError> {
        self.next_frame_using(StoreReader::read)
    }
    fn next_frame_using(
        &mut self,
        mut read: impl FnMut(&mut StoreReader<'r>, &mut [u8]) -> io::Result<usize>,
    ) -> Result<Option<Frame<'_>>, RecoveryInputError> {
        if self.failed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe).into());
        }
        if self.file.position() == self.file.len() {
            return Ok(None);
        }
        self.failed = true;
        let start = self.file.position();
        let remaining = self
            .file
            .len()
            .checked_sub(start)
            .ok_or(FormatError::InvalidValue)?;
        let next = self.through.successor()?;
        let used = start
            .checked_sub(JOURNAL_HEADER_BYTES as u64)
            .ok_or(FormatError::InvalidValue)?;
        let available = (MAX_JOURNAL_FRAME_BYTES as u64)
            .checked_sub(used)
            .ok_or(FormatError::Limit)?;
        if available < MIN_FRAME_BYTES as u64 || self.operations == MAX_JOURNAL_OPERATIONS {
            return Err(FormatError::Limit.into());
        }
        let mut attempts = MAX_READ_CALLS;
        if remaining < FRAME_HEADER_BYTES as u64 {
            let tail = self
                .scratch
                .get_mut(..usize::try_from(remaining).map_err(|_| FormatError::Overflow)?)
                .ok_or(FormatError::Limit)?;
            fill_exact_using(&mut self.file, tail, &mut attempts, &mut read)?;
            self.failed = false;
            return Ok(None);
        }
        let prefix = self
            .scratch
            .get_mut(..FRAME_HEADER_BYTES)
            .ok_or(FormatError::Limit)?;
        fill_exact_using(&mut self.file, prefix, &mut attempts, &mut read)?;
        let header =
            FrameHeader::decode(self.crypto, prefix).map_err(|e| StreamError::Frame(e.into()))?;
        if header.sequence != next {
            return Err(StreamError::Frame(FormatError::InvalidValue.into()).into());
        }
        let declared_bytes = start
            .checked_sub(JOURNAL_HEADER_BYTES as u64)
            .and_then(|n| n.checked_add(header.frame_bytes as u64))
            .ok_or(FormatError::Overflow)?;
        let operations = self
            .operations
            .checked_add(header.operations)
            .ok_or(FormatError::Overflow)?;
        if declared_bytes > MAX_JOURNAL_FRAME_BYTES as u64 || operations > MAX_JOURNAL_OPERATIONS {
            return Err(FormatError::Limit.into());
        }
        let incomplete = header.frame_bytes as u64 > remaining;
        let consume = if incomplete {
            usize::try_from(remaining).map_err(|_| FormatError::Overflow)?
        } else {
            header.frame_bytes
        };
        let bytes = self.scratch.get_mut(..consume).ok_or(FormatError::Limit)?;
        fill_exact_using(
            &mut self.file,
            bytes
                .get_mut(FRAME_HEADER_BYTES..)
                .ok_or(FormatError::Limit)?,
            &mut attempts,
            &mut read,
        )?;
        if incomplete {
            self.failed = false;
            return Ok(None);
        }
        let frame = self.verifier.push(bytes)?;
        self.through = frame.header().sequence;
        self.operations = operations;
        self.failed = false;
        Ok(Some(frame))
    }
    pub fn finish(self) -> Result<ScannedJournal<'r>, RecoveryInputError> {
        if self.failed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe).into());
        }
        let summary = self.verifier.finish()?;
        let file = self.file.finish()?;
        let prefix = u64::try_from(summary.file_bytes()?).map_err(|_| FormatError::Overflow)?;
        if prefix > file.len() {
            return Err(FormatError::InvalidValue.into());
        }
        Ok(ScannedJournal {
            file,
            summary,
            prefix,
            current: self.current,
        })
    }
}
/// Complete physical scan with a valid prefix; neither repair nor final-view validity.
#[derive(Debug)]
pub struct ScannedJournal<'r> {
    file: CompleteFile<'r>,
    summary: Summary,
    prefix: u64,
    current: Current,
}
impl ScannedJournal<'_> {
    pub fn file(&self) -> &CompleteFile<'_> {
        &self.file
    }
    pub fn summary(&self) -> Summary {
        self.summary
    }
    pub fn valid_bytes(&self) -> u64 {
        self.prefix
    }
    pub fn has_incomplete_tail(&self) -> bool {
        self.file.len() != self.prefix
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod fixture {
    use super::*;
    use crate::{ids::AccountId, store_paths::Name};
    use std::{fs, os::unix::fs::PermissionsExt};
    pub(super) const ACCOUNT: AccountId = AccountId::from_bytes([0x33; 16]);
    pub(super) fn hex(text: &str) -> Vec<u8> {
        let text: String = text.split_whitespace().collect();
        let (pairs, rest) = text.as_bytes().as_chunks::<2>();
        assert!(rest.is_empty());
        pairs
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    pub(super) fn journal() -> Vec<u8> {
        let mut bytes = hex(include_str!(
            "../../../tests/fixtures/format-v1/active-journal-two.hex"
        ));
        bytes.extend_from_slice(&hex(include_str!(
            "../../../tests/fixtures/format-v1/frame-delete-change.hex"
        )));
        bytes
    }
    pub(super) fn write(root: &LockedRoot, bytes: &[u8]) {
        let name = Name::account(ACCOUNT, AccountEntry::Journal(Number::new(2).unwrap())).unwrap();
        let mut buffer = [0; super::super::super::MAX_PATH_BYTES];
        let path = root.root.directory.join(&name, &mut buffer).unwrap();
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    pub(crate) struct Bytes {
        format: Vec<u8>,
        current: Vec<u8>,
        manifest: Vec<u8>,
    }
    impl Bytes {
        pub(super) fn checkpoint(&mut self, through: Sequence) {
            use crate::format::{container::Current, manifest, Table, TABLE_COUNT};
            use td_crypto::Digest;
            let selected = self.selection();
            let manifest = selected.manifest();
            let tables = std::array::from_fn::<_, TABLE_COUNT, _>(|index| {
                manifest
                    .table(Table::from_tag((index + 1) as u16).unwrap())
                    .unwrap()
            });
            let header = manifest::Header {
                through,
                ..manifest.header()
            };
            let mut output = vec![0; crate::format::MAX_MANIFEST_BYTES];
            let len =
                manifest::encode(&td_crypto::Provider, header, &tables, &[], &mut output).unwrap();
            output.truncate(len);
            self.manifest = output;
            let mut digest = td_crypto::Provider.sha256().unwrap();
            digest.update(&self.manifest).unwrap();
            let current = Current {
                manifest_digest: digest.finish().unwrap(),
                ..Current::decode(&td_crypto::Provider, &self.current).unwrap()
            };
            current
                .encode(&td_crypto::Provider, &mut self.current)
                .unwrap();
        }
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
    pub(crate) fn bytes() -> Bytes {
        Bytes {
            format: hex(include_str!("../../../tests/fixtures/format-v1/format.hex")),
            current: hex(include_str!(
                "../../../tests/fixtures/format-v1/current-history.hex"
            )),
            manifest: hex(include_str!(
                "../../../tests/fixtures/format-v1/manifest-history.hex"
            )),
        }
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::super::tests::Fixture;
    use super::{fixture::*, *};
    use crate::format::{container::Error as ContainerError, frame::DecodeError};
    use td_crypto::Provider;
    const MAX_BYTES: u64 = (JOURNAL_HEADER_BYTES + MAX_JOURNAL_FRAME_BYTES) as u64;
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
        write(&root, &journal());
        (root, bytes())
    }
    fn scratch(buffer: &mut [u8]) -> &mut [u8; MAX_FRAME_BYTES] {
        buffer.try_into().unwrap()
    }
    #[test]
    fn stopped_scan_requires_header_and_retains_complete_frames() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        let data = journal();
        for length in 0..JOURNAL_HEADER_BYTES {
            write(&root, data.get(..length).unwrap());
            assert!(matches!(
                root.scan_active_journal(
                    &Provider,
                    bytes.selection(),
                    MAX_BYTES,
                    scratch(&mut buffer)
                ),
                Err(RecoveryInputError::Stream(StreamError::Journal(
                    ContainerError::Format(FormatError::Truncated)
                )))
            ));
        }
        for length in [96, 256] {
            write(&root, data.get(..length).unwrap());
            let mut input = root
                .scan_active_journal(
                    &Provider,
                    bytes.selection(),
                    MAX_BYTES,
                    scratch(&mut buffer),
                )
                .unwrap();
            if length > 96 {
                assert_eq!(
                    input
                        .next_frame()
                        .unwrap()
                        .unwrap()
                        .header()
                        .sequence
                        .number(),
                    2
                );
            }
            assert!(input.next_frame().unwrap().is_none());
            let scan = input.finish().unwrap();
            assert_eq!(scan.valid_bytes(), length as u64);
            assert!(!scan.has_incomplete_tail());
        }
    }
    #[test]
    fn stopped_open_binds_selected_identity_and_admits_whole_extent() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        assert!(
            matches!(root.scan_active_journal(&Provider, bytes.selection(), 95, scratch(&mut buffer)),
            Err(RecoveryInputError::Io(e)) if e.kind() == io::ErrorKind::InvalidInput)
        );
        assert!(
            matches!(root.scan_active_journal(&Provider, bytes.selection(), 255, scratch(&mut buffer)),
            Err(RecoveryInputError::Io(e)) if e.kind() == io::ErrorKind::InvalidData)
        );
        let original = journal();
        let header = JournalHeader::decode(&Provider, original.get(..96).unwrap()).unwrap();
        for changed in [
            JournalHeader {
                account: crate::ids::AccountId::from_bytes([9; 16]),
                ..header
            },
            JournalHeader {
                epoch: crate::ids::StoreEpoch::from_bytes([9; 16]),
                ..header
            },
            JournalHeader {
                segment: 1,
                ..header
            },
            JournalHeader {
                base: Sequence::from_u64(0),
                ..header
            },
        ] {
            let mut data = original.clone();
            changed
                .encode(&Provider, data.get_mut(..96).unwrap())
                .unwrap();
            write(&root, &data);
            assert!(matches!(
                root.scan_active_journal(
                    &Provider,
                    bytes.selection(),
                    MAX_BYTES,
                    scratch(&mut buffer)
                ),
                Err(RecoveryInputError::Stream(StreamError::Journal(
                    ContainerError::Format(FormatError::InvalidValue)
                )))
            ));
        }
    }
    #[test]
    fn physical_short_headers_bodies_and_footers_are_observed_without_repair() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        let data = journal();
        for tail in 1..160 {
            write(&root, data.get(..96 + tail).unwrap());
            let mut input = root
                .scan_active_journal(
                    &Provider,
                    bytes.selection(),
                    MAX_BYTES,
                    scratch(&mut buffer),
                )
                .unwrap();
            assert!(input.next_frame().unwrap().is_none());
            let scan = input.finish().unwrap();
            assert!(scan.has_incomplete_tail());
            assert_eq!(scan.valid_bytes(), 96);
            assert_eq!(scan.file().len(), (96 + tail) as u64);
            assert_eq!(scan.summary().through().number(), 1);
            let mut observed = [0; 1];
            assert_eq!(
                scan.file()
                    .read_at((95 + tail) as u64, &mut observed)
                    .unwrap(),
                1
            );
            assert_eq!(observed.first(), data.get(95 + tail));
        }
        let mut data = journal();
        let mut second = data.get(96..).unwrap().to_vec();
        crate::format::frame::seal(&Provider, Sequence::from_u64(3), 2, &mut second).unwrap();
        data.extend_from_slice(second.get(..100).unwrap());
        write(&root, &data);
        let mut input = root
            .scan_active_journal(
                &Provider,
                bytes.selection(),
                MAX_BYTES,
                scratch(&mut buffer),
            )
            .unwrap();
        assert!(input.next_frame().unwrap().is_some());
        assert!(input.next_frame().unwrap().is_none());
        let scan = input.finish().unwrap();
        assert_eq!(scan.valid_bytes(), 256);
        assert_eq!(scan.summary().through().number(), 2);
        assert!(scan.has_incomplete_tail());
    }
    #[test]
    fn complete_corruption_and_invalid_short_tail_header_cannot_finish() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        for mode in 0..6 {
            let mut data = journal();
            match mode {
                0 => *data.get_mut(159).unwrap() ^= 1,
                1 => *data.last_mut().unwrap() ^= 1,
                2 => {
                    crate::format::frame::seal(
                        &Provider,
                        Sequence::from_u64(3),
                        2,
                        data.get_mut(96..).unwrap(),
                    )
                    .unwrap();
                }
                3 => {
                    *data.get_mut(159).unwrap() ^= 1;
                    data.truncate(170);
                }
                _ => {
                    let sequence = if mode == 4 { 3 } else { 1 };
                    crate::format::frame::seal(
                        &Provider,
                        Sequence::from_u64(sequence),
                        2,
                        data.get_mut(96..).unwrap(),
                    )
                    .unwrap();
                    data.truncate(196);
                }
            }
            write(&root, &data);
            let mut input = root
                .scan_active_journal(
                    &Provider,
                    bytes.selection(),
                    MAX_BYTES,
                    scratch(&mut buffer),
                )
                .unwrap();
            let expected = if matches!(mode, 2 | 4 | 5) {
                ContainerError::Format(FormatError::InvalidValue)
            } else {
                ContainerError::Checksum
            };
            assert!(
                matches!(input.next_frame(), Err(RecoveryInputError::Stream(StreamError::Frame(DecodeError::Invalid(e)))) if e == expected)
            );
            assert_eq!(input.file.position(), if mode == 1 { 256 } else { 160 });
            assert!(input.is_failed());
            assert!(
                matches!(input.next_frame(),Err(RecoveryInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
            );
            assert!(
                matches!(input.finish(),Err(RecoveryInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
            );
        }
    }
    #[test]
    fn impossible_declared_operation_budget_is_not_a_repairable_short_tail() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        let original = journal();
        let operations = original.get(160..216).unwrap();
        let mut frame = vec![0; FRAME_HEADER_BYTES];
        for _ in 0..2048 {
            frame.extend_from_slice(operations);
        }
        frame.resize(frame.len() + crate::format::FRAME_FOOTER_BYTES, 0);
        let mut data = original.get(..96).unwrap().to_vec();
        for sequence in [2, 3] {
            crate::format::frame::seal(&Provider, Sequence::from_u64(sequence), 4096, &mut frame)
                .unwrap();
            data.extend_from_slice(&frame);
        }
        let mut last = original.get(96..).unwrap().to_vec();
        crate::format::frame::seal(&Provider, Sequence::from_u64(4), 2, &mut last).unwrap();
        for tail in [1, 63, 80] {
            let mut journal = data.clone();
            journal.extend_from_slice(last.get(..tail).unwrap());
            write(&root, &journal);
            let mut input = root
                .scan_active_journal(
                    &Provider,
                    bytes.selection(),
                    MAX_BYTES,
                    scratch(&mut buffer),
                )
                .unwrap();
            for _ in 0..2 {
                assert!(input.next_frame().unwrap().is_some());
            }
            let mut calls = 0;
            assert!(matches!(
                input.next_frame_using(|file, output| {
                    calls += 1;
                    file.read(output)
                }),
                Err(RecoveryInputError::Stream(StreamError::Journal(
                    ContainerError::Format(FormatError::Limit)
                )))
            ));
            assert_eq!(calls, 0);
            assert_eq!(input.file.position(), data.len() as u64);
            assert!(input.is_failed());
            assert!(
                matches!(input.finish(), Err(RecoveryInputError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe)
            );
        }
    }
    fn near_full_journal(spare: usize) -> Vec<u8> {
        let mut data = journal().get(..96).unwrap().to_vec();
        for sequence in 2..=5 {
            let length = MAX_FRAME_BYTES - if sequence == 5 { spare } else { 0 };
            let mut frame = vec![0; FRAME_HEADER_BYTES];
            let mut remaining = length - FRAME_HEADER_BYTES - crate::format::FRAME_FOOTER_BYTES;
            let mut count = 0;
            while remaining > 0 {
                let mut step = remaining.min(1036);
                if remaining > step && remaining - step < 33 {
                    step -= 33 - (remaining - step);
                }
                let text = "x".repeat(step - 32);
                let key = crate::format::key::Key::ThreadAnchor(
                    &text,
                    crate::ids::EmailId::from_bytes([9; 16]),
                );
                let mut key_bytes = vec![0; step - 12];
                key.encode(&mut key_bytes).unwrap();
                let operation = crate::format::operation::Operation::delete(
                    crate::format::Table::ThreadAnchors,
                    &key_bytes,
                )
                .unwrap();
                let start = frame.len();
                frame.resize(start + step, 0);
                operation.encode(frame.get_mut(start..).unwrap()).unwrap();
                count += 1;
                remaining -= step;
            }
            frame.resize(length, 0);
            crate::format::frame::seal(&Provider, Sequence::from_u64(sequence), count, &mut frame)
                .unwrap();
            data.extend_from_slice(&frame);
        }
        data
    }
    #[test]
    fn short_headers_refuse_exhausted_sequence_and_minimum_frame_capacity() {
        let fixture = Fixture::new();
        let (root, mut bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        let mut data = near_full_journal(MIN_FRAME_BYTES - 1);
        let boundary = data.len() as u64;
        data.push(0);
        write(&root, &data);
        let mut input = root
            .scan_active_journal(
                &Provider,
                bytes.selection(),
                MAX_BYTES,
                scratch(&mut buffer),
            )
            .unwrap();
        for _ in 0..4 {
            assert!(input.next_frame().unwrap().is_some());
        }
        let mut calls = 0;
        assert!(matches!(
            input.next_frame_using(|file, output| {
                calls += 1;
                file.read(output)
            }),
            Err(RecoveryInputError::Stream(StreamError::Journal(
                ContainerError::Format(FormatError::Limit)
            )))
        ));
        assert_eq!(calls, 0);
        assert_eq!(input.file.position(), boundary);
        assert!(
            matches!(input.finish(), Err(RecoveryInputError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe)
        );
        bytes.checkpoint(Sequence::from_u64(u64::MAX));
        let header = JournalHeader {
            base: Sequence::from_u64(u64::MAX),
            ..JournalHeader::decode(&Provider, journal().get(..96).unwrap()).unwrap()
        };
        let mut data = vec![0; 97];
        header
            .encode(&Provider, data.get_mut(..96).unwrap())
            .unwrap();
        write(&root, &data);
        let mut input = root
            .scan_active_journal(
                &Provider,
                bytes.selection(),
                MAX_BYTES,
                scratch(&mut buffer),
            )
            .unwrap();
        assert!(matches!(
            input.next_frame_using(|_, _| {
                calls += 1;
                Err(io::ErrorKind::Other.into())
            }),
            Err(RecoveryInputError::Stream(StreamError::Journal(
                ContainerError::Format(FormatError::Exhausted)
            )))
        ));
        assert_eq!(calls, 0);
        assert_eq!(input.file.position(), 96);
        assert!(
            matches!(input.finish(), Err(RecoveryInputError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe)
        );
    }
    #[test]
    fn short_body_declared_bytes_and_opening_format_cap_both_refuse() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        let mut data = near_full_journal(159);
        let boundary = data.len() as u64;
        let mut frame = journal().get(96..).unwrap().to_vec();
        crate::format::frame::seal(&Provider, Sequence::from_u64(6), 2, &mut frame).unwrap();
        data.extend_from_slice(frame.get(..80).unwrap());
        write(&root, &data);
        let mut input = root
            .scan_active_journal(
                &Provider,
                bytes.selection(),
                MAX_BYTES,
                scratch(&mut buffer),
            )
            .unwrap();
        for _ in 0..4 {
            assert!(input.next_frame().unwrap().is_some());
        }
        assert!(matches!(
            input.next_frame(),
            Err(RecoveryInputError::Stream(StreamError::Journal(
                ContainerError::Format(FormatError::Limit)
            )))
        ));
        assert_eq!(input.file.position(), boundary + FRAME_HEADER_BYTES as u64);
        assert!(
            matches!(input.finish(), Err(RecoveryInputError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe)
        );
        let name = crate::store_paths::Name::account(
            ACCOUNT,
            AccountEntry::Journal(Number::new(2).unwrap()),
        )
        .unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(fixture.path.join(name.as_path().unwrap()))
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert!(
            matches!(root.scan_active_journal(&Provider, bytes.selection(), MAX_BYTES + 1, scratch(&mut buffer)),
            Err(RecoveryInputError::Io(e)) if e.kind() == io::ErrorKind::InvalidData)
        );
    }
    #[test]
    fn stopped_scan_changes_and_read_budget_failures_grant_no_tail_evidence() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        for length in [0, 257] {
            write(&root, &journal());
            let mut input = root
                .scan_active_journal(
                    &Provider,
                    bytes.selection(),
                    MAX_BYTES,
                    scratch(&mut buffer),
                )
                .unwrap();
            assert!(input.next_frame().unwrap().is_some());
            let mut changed = journal();
            changed.resize(length, 0);
            write(&root, &changed);
            assert!(
                matches!(input.finish(),Err(RecoveryInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidData)
            );
        }
        write(&root, &journal());
        let mut input = root
            .scan_active_journal(
                &Provider,
                bytes.selection(),
                MAX_BYTES,
                scratch(&mut buffer),
            )
            .unwrap();
        let mut calls = 0;
        assert!(
            matches!(input.next_frame_using(|file,output| { calls+=1;file.read(output.get_mut(..1).unwrap()) }),Err(RecoveryInputError::Io(e)) if e.kind()==io::ErrorKind::WouldBlock)
        );
        assert_eq!(calls, MAX_READ_CALLS);
        assert_eq!(input.file.position(), 160);
        assert!(input.is_failed());
        assert!(
            matches!(input.finish(),Err(RecoveryInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
        );
        write(&root, &journal());
        let mut input = root
            .scan_active_journal(
                &Provider,
                bytes.selection(),
                MAX_BYTES,
                scratch(&mut buffer),
            )
            .unwrap();
        write(&root, &[]);
        assert!(
            matches!(input.next_frame(),Err(RecoveryInputError::Io(e)) if e.kind()==io::ErrorKind::UnexpectedEof)
        );
        assert!(
            matches!(input.finish(),Err(RecoveryInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
        );
    }
}

#[cfg(test)]
pub(crate) use fixture::{bytes as prepare_probe, Bytes as ProbeBytes};
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) fn probe(root: &LockedRoot, bytes: &ProbeBytes, scratch: &mut [u8; MAX_FRAME_BYTES]) {
    // The active-input fixture installs segment 2 plus a 17-byte incomplete suffix.
    let selection = bytes.selection();
    let cap = (JOURNAL_HEADER_BYTES + MAX_JOURNAL_FRAME_BYTES) as u64;
    let mut input = root
        .scan_active_journal(&td_crypto::Provider, selection, cap, scratch)
        .unwrap();
    assert_eq!(
        input
            .next_frame()
            .unwrap()
            .unwrap()
            .header()
            .sequence
            .number(),
        2
    );
    assert!(input.next_frame().unwrap().is_none());
    let scan = input.finish().unwrap();
    assert_eq!(scan.valid_bytes(), 256);
    assert_eq!(scan.file().len(), 273);
    assert!(scan.has_incomplete_tail());
    assert_eq!(scan.file().read_at(272, &mut [0; 1]).unwrap(), 1);
    assert!(
        matches!(root.scan_active_journal(&td_crypto::Provider,selection,255,scratch),Err(RecoveryInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidData)
    );
    assert!(
        matches!(root.scan_active_journal(&td_crypto::Provider,selection,cap,scratch).unwrap().finish(),Err(RecoveryInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidInput)
    );
    let mut input = root
        .scan_active_journal(&td_crypto::Provider, selection, cap, scratch)
        .unwrap();
    assert!(
        matches!(input.next_frame_using(|_,_|Err(io::ErrorKind::Interrupted.into())),Err(RecoveryInputError::Io(e)) if e.kind()==io::ErrorKind::Interrupted)
    );
    assert!(input.is_failed());
    assert!(
        matches!(input.finish(),Err(RecoveryInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
    );
}
