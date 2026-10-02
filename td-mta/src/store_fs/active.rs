//! Checked captured active frames; actual runtime pins and replay remain external.
pub use super::journal_input::Error as ActiveInputError;
use super::{journal_input::FrameInput, CompletePrefix, LockedRoot, PrefixReader};
use crate::{
    format::{
        bindings::Selection, container::JournalHeader, frame::Frame, journal_stream::Summary,
        Error as FormatError, Sequence, JOURNAL_HEADER_BYTES, MAX_FRAME_BYTES,
        MAX_JOURNAL_FRAME_BYTES, MAX_JOURNAL_OPERATIONS, MIN_FRAME_BYTES,
    },
    ports::{Crypto, ViewIdentity},
    store_paths::Number,
};
use std::io;

pub struct ActiveInput<'r, 'c, 'm, 'b, C: Crypto> {
    stream: FrameInput<'c, 'b, C, PrefixReader<'r>>,
    selection: Selection<'m>,
    through: Sequence,
    bytes: u64,
}
impl LockedRoot {
    /// Caller holds actual prefix ownership/barriers; ViewIdentity is data, not a pin.
    /// This checks active fields only. History retention/floor validation is separate.
    pub fn open_active_prefix<'r, 'c, 'm, 'b, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'m>,
        view: ViewIdentity,
        max_bytes: u64,
        scratch: &'b mut [u8; MAX_FRAME_BYTES],
    ) -> Result<ActiveInput<'r, 'c, 'm, 'b, C>, ActiveInputError> {
        validate_view(selection, view)?;
        if view.committed_offset > max_bytes {
            return Err(io::Error::from(io::ErrorKind::InvalidInput).into());
        }
        let selected = selection.manifest().header();
        let file = self.open_journal_prefix(
            selected.account,
            Number::new(selected.active_segment).map_err(|_| FormatError::InvalidValue)?,
            view.committed_offset,
            (JOURNAL_HEADER_BYTES + MAX_JOURNAL_FRAME_BYTES) as u64,
        )?;
        let expected = JournalHeader {
            account: selected.account,
            epoch: selected.epoch,
            segment: selected.active_segment,
            base: selected.through,
        };
        let stream = FrameInput::new(crypto, file, expected, scratch)?;
        Ok(ActiveInput {
            stream,
            selection,
            through: view.committed_sequence,
            bytes: view.committed_offset,
        })
    }
}
pub(super) fn validate_view(
    selection: Selection<'_>,
    view: ViewIdentity,
) -> Result<(), FormatError> {
    let selected = selection.manifest().header();
    if view.account != selected.account
        || view.epoch != selected.epoch
        || view.generation != selected.generation
        || view.checkpoint != selected.through
        || view.segment != selected.active_segment
    {
        return Err(FormatError::InvalidValue);
    }
    let payload = view
        .committed_offset
        .checked_sub(JOURNAL_HEADER_BYTES as u64)
        .ok_or(FormatError::InvalidValue)?;
    let frames = view
        .committed_sequence
        .number()
        .checked_sub(view.checkpoint.number())
        .ok_or(FormatError::InvalidValue)?;
    if payload > MAX_JOURNAL_FRAME_BYTES as u64 || frames > MAX_JOURNAL_OPERATIONS as u64 {
        return Err(FormatError::Limit);
    }
    let minimum = frames
        .checked_mul(MIN_FRAME_BYTES as u64)
        .ok_or(FormatError::Overflow)?;
    let maximum = frames
        .checked_mul(MAX_FRAME_BYTES as u64)
        .ok_or(FormatError::Overflow)?;
    if payload < minimum || payload > maximum {
        return Err(FormatError::InvalidValue);
    }
    Ok(())
}
impl<'r, C: Crypto> ActiveInput<'r, '_, '_, '_, C> {
    pub fn is_failed(&self) -> bool {
        self.stream.is_failed()
    }
    /// One provisional frame; None alone does not establish captured completion.
    pub fn next_frame(&mut self) -> Result<Option<Frame<'_>>, ActiveInputError> {
        self.stream.next_frame()
    }
    pub fn finish(self) -> Result<CompleteActive<'r>, ActiveInputError> {
        let (file, summary) = self.stream.finish()?;
        self.selection
            .check_active_prefix(self.through, self.bytes, summary)?;
        Ok(CompleteActive { file, summary })
    }
}
#[derive(Debug)]
pub struct CompleteActive<'r> {
    file: CompletePrefix<'r>,
    summary: Summary,
}
impl CompleteActive<'_> {
    pub fn file(&self) -> &CompletePrefix<'_> {
        &self.file
    }
    pub fn summary(&self) -> Summary {
        self.summary
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) mod fixture {
    use super::*;
    use crate::{
        ids::AccountId,
        store_paths::{AccountEntry, Name},
    };
    use std::{fs, os::unix::fs::PermissionsExt};
    pub(crate) const ACCOUNT: AccountId = AccountId::from_bytes([0x33; 16]);
    pub(crate) fn hex(text: &str) -> Vec<u8> {
        let text: String = text.split_whitespace().collect();
        let (pairs, rest) = text.as_bytes().as_chunks::<2>();
        assert!(rest.is_empty());
        pairs
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    pub(crate) fn journal() -> Vec<u8> {
        let mut bytes = hex(include_str!(
            "../../tests/fixtures/format-v1/active-journal-two.hex"
        ));
        bytes.extend_from_slice(&hex(include_str!(
            "../../tests/fixtures/format-v1/frame-delete-change.hex"
        )));
        bytes
    }
    pub(crate) fn write(root: &LockedRoot, bytes: &[u8]) {
        let name = Name::account(ACCOUNT, AccountEntry::Journal(Number::new(2).unwrap())).unwrap();
        let mut buffer = [0; super::super::MAX_PATH_BYTES];
        let path = root.root.directory.join(&name, &mut buffer).unwrap();
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    pub(crate) fn append(root: &LockedRoot, bytes: &[u8]) {
        use std::io::Write;
        let name = Name::account(ACCOUNT, AccountEntry::Journal(Number::new(2).unwrap())).unwrap();
        let mut buffer = [0; super::super::MAX_PATH_BYTES];
        let path = root.root.directory.join(&name, &mut buffer).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }
    pub(crate) struct Bytes {
        format: Vec<u8>,
        current: Vec<u8>,
        manifest: Vec<u8>,
    }
    impl Bytes {
        pub(crate) fn selection(&self) -> Selection<'_> {
            Selection::decode(
                &td_crypto::Provider,
                ACCOUNT,
                &self.format,
                &self.current,
                &self.manifest,
            )
            .unwrap()
        }
        pub(crate) fn view(&self, through: u64, length: u64) -> ViewIdentity {
            let selected = self.selection().manifest().header();
            ViewIdentity {
                account: selected.account,
                epoch: selected.epoch,
                generation: selected.generation,
                checkpoint: selected.through,
                segment: selected.active_segment,
                committed_offset: length,
                committed_sequence: Sequence::from_u64(through),
                history_floor: Sequence::from_u64(0),
            }
        }
    }
    pub(crate) fn prepare(root: &LockedRoot) -> Bytes {
        let mut data = journal();
        data.extend_from_slice(b"incomplete append");
        write(root, &data);
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
    let maximum = (JOURNAL_HEADER_BYTES + MAX_JOURNAL_FRAME_BYTES) as u64;
    let mut input = root
        .open_active_prefix(
            &td_crypto::Provider,
            selection,
            bytes.view(1, 96),
            96,
            scratch,
        )
        .unwrap();
    assert!(input.next_frame().unwrap().is_none());
    let complete = input.finish().unwrap();
    assert_eq!(complete.summary().through().number(), 1);
    assert_eq!(complete.file().len(), 96);
    let mut input = root
        .open_active_prefix(
            &td_crypto::Provider,
            selection,
            bytes.view(2, 256),
            256,
            scratch,
        )
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
    assert_eq!(input.finish().unwrap().file().len(), 256);
    assert!(
        matches!(root.open_active_prefix(&td_crypto::Provider, selection, bytes.view(2,256), 255, scratch), Err(ActiveInputError::Io(e)) if e.kind() == io::ErrorKind::InvalidInput)
    );
    let mut wrong = bytes.view(2, 256);
    wrong.generation += 1;
    assert!(matches!(
        root.open_active_prefix(&td_crypto::Provider, selection, wrong, maximum, scratch),
        Err(ActiveInputError::Stream(_))
    ));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::tests::Fixture;
    use super::{fixture::*, *};
    use crate::{
        format::{
            container::{Error as ContainerError, JournalHeader},
            frame::DecodeError,
            journal_stream::Error as StreamError,
        },
        ids::{AccountId, StoreEpoch},
        store_paths::{AccountEntry, Name},
    };
    use td_crypto::Provider;
    const MAX_BYTES: u64 = (JOURNAL_HEADER_BYTES + MAX_JOURNAL_FRAME_BYTES) as u64;
    fn setup(fixture: &Fixture) -> (LockedRoot, ProbeBytes) {
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Metadata,
            AccountEntry::Journals,
        ] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
        let bytes = prepare_probe(&root);
        (root, bytes)
    }
    fn scratch(buffer: &mut [u8]) -> &mut [u8; MAX_FRAME_BYTES] {
        buffer.try_into().unwrap()
    }
    fn two_frames() -> Vec<u8> {
        let mut data = journal();
        let mut second = hex(include_str!(
            "../../tests/fixtures/format-v1/frame-delete-change.hex"
        ));
        crate::format::frame::seal(&Provider, Sequence::from_u64(3), 2, &mut second).unwrap();
        data.extend_from_slice(&second);
        data
    }
    #[test]
    fn active_prefixes_ignore_appends_and_reuse_frame_scratch() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        probe(&root, &bytes, scratch(&mut buffer));
        write(&root, &journal());
        let mut input = root
            .open_active_prefix(
                &Provider,
                bytes.selection(),
                bytes.view(2, 256),
                MAX_BYTES,
                scratch(&mut buffer),
            )
            .unwrap();
        assert!(input.next_frame().unwrap().is_some());
        let mut data = two_frames();
        data.extend_from_slice(b"partial");
        append(&root, data.get(256..).unwrap());
        assert!(input.next_frame().unwrap().is_none());
        assert_eq!(input.finish().unwrap().summary().through().number(), 2);
        let mut input = root
            .open_active_prefix(
                &Provider,
                bytes.selection(),
                bytes.view(3, 416),
                MAX_BYTES,
                scratch(&mut buffer),
            )
            .unwrap();
        for sequence in [2, 3] {
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
        assert_eq!(complete.summary().operations(), 4);
        assert_eq!(complete.file().len(), 416);
        let name = Name::account(ACCOUNT, AccountEntry::Journal(Number::new(2).unwrap())).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(fixture.path.join(name.as_path().unwrap()))
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert!(
            matches!(root.open_active_prefix(&Provider, bytes.selection(), bytes.view(2,256),256,scratch(&mut buffer)),
            Err(ActiveInputError::Io(e)) if e.kind() == io::ErrorKind::InvalidData)
        );
    }
    #[test]
    fn active_view_identity_and_range_refuse_before_file_lookup() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let view = bytes.view(2, 256);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        let missing =
            Name::account(ACCOUNT, AccountEntry::Journal(Number::new(2).unwrap())).unwrap();
        std::fs::remove_file(fixture.path.join(missing.as_path().unwrap())).unwrap();
        for invalid in [
            ViewIdentity {
                account: AccountId::from_bytes([9; 16]),
                ..view
            },
            ViewIdentity {
                epoch: StoreEpoch::from_bytes([9; 16]),
                ..view
            },
            ViewIdentity {
                generation: 3,
                ..view
            },
            ViewIdentity {
                checkpoint: Sequence::from_u64(0),
                ..view
            },
            ViewIdentity { segment: 1, ..view },
            ViewIdentity {
                committed_offset: 95,
                ..view
            },
            ViewIdentity {
                committed_offset: 96,
                ..view
            },
            ViewIdentity {
                committed_sequence: Sequence::from_u64(0),
                ..view
            },
            ViewIdentity {
                committed_sequence: Sequence::from_u64(1),
                ..view
            },
            ViewIdentity {
                committed_sequence: Sequence::from_u64(3),
                ..view
            },
        ] {
            assert!(matches!(
                root.open_active_prefix(
                    &Provider,
                    bytes.selection(),
                    invalid,
                    MAX_BYTES,
                    scratch(&mut buffer)
                ),
                Err(ActiveInputError::Stream(StreamError::Journal(
                    ContainerError::Format(FormatError::InvalidValue)
                )))
            ));
        }
        for invalid in [
            ViewIdentity {
                committed_offset: MAX_BYTES + 1,
                ..view
            },
            ViewIdentity {
                committed_offset: u64::MAX,
                ..view
            },
            ViewIdentity {
                committed_sequence: Sequence::from_u64(u64::MAX),
                ..view
            },
        ] {
            assert!(matches!(
                root.open_active_prefix(
                    &Provider,
                    bytes.selection(),
                    invalid,
                    MAX_BYTES,
                    scratch(&mut buffer)
                ),
                Err(ActiveInputError::Stream(StreamError::Journal(
                    ContainerError::Format(FormatError::Limit)
                )))
            ));
        }
        assert!(
            matches!(root.open_active_prefix(&Provider,bytes.selection(),view,255,scratch(&mut buffer)),
            Err(ActiveInputError::Io(e)) if e.kind() == io::ErrorKind::InvalidInput)
        );
        assert!(
            matches!(root.open_active_prefix(&Provider,bytes.selection(),view,MAX_BYTES,scratch(&mut buffer)),
            Err(ActiveInputError::Io(e)) if e.kind() == io::ErrorKind::NotFound)
        );
    }
    #[test]
    fn active_header_identity_is_checked_before_frames() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let original = journal();
        let header = JournalHeader::decode(&Provider, original.get(..96).unwrap()).unwrap();
        let mut buffer = vec![0; MAX_FRAME_BYTES];
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
                root.open_active_prefix(
                    &Provider,
                    bytes.selection(),
                    bytes.view(2, 256),
                    MAX_BYTES,
                    scratch(&mut buffer)
                ),
                Err(ActiveInputError::Stream(StreamError::Journal(
                    ContainerError::Format(FormatError::InvalidValue)
                )))
            ));
        }
    }
    #[test]
    fn active_frame_errors_and_short_physical_reads_retire_input() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        for mode in 0..3 {
            write(&root, &journal());
            let endpoint = if mode == 0 { 255 } else { 256 };
            let mut input = root
                .open_active_prefix(
                    &Provider,
                    bytes.selection(),
                    bytes.view(2, endpoint),
                    MAX_BYTES,
                    scratch(&mut buffer),
                )
                .unwrap();
            let mut changed = journal();
            if mode == 1 {
                changed.truncate(96);
            }
            if mode == 2 {
                *changed.last_mut().unwrap() ^= 1;
            }
            write(&root, &changed);
            let error = input.next_frame();
            match mode {
                0 => assert!(matches!(
                    error,
                    Err(ActiveInputError::Stream(StreamError::Frame(
                        DecodeError::Invalid(ContainerError::Format(FormatError::InvalidValue))
                    )))
                )),
                1 => assert!(
                    matches!(error,Err(ActiveInputError::Io(e)) if e.kind()==io::ErrorKind::UnexpectedEof)
                ),
                _ => assert!(matches!(
                    error,
                    Err(ActiveInputError::Stream(StreamError::Frame(
                        DecodeError::Invalid(ContainerError::Checksum)
                    )))
                )),
            }
            assert!(input.is_failed());
            assert!(
                matches!(input.next_frame(),Err(ActiveInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
            );
            assert!(
                matches!(input.finish(),Err(ActiveInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
            );
        }
    }
    #[test]
    fn active_completion_requires_captured_sequence_and_present_prefix() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut buffer = vec![0; MAX_FRAME_BYTES];
        write(&root, &two_frames());
        let mut input = root
            .open_active_prefix(
                &Provider,
                bytes.selection(),
                bytes.view(2, 416),
                MAX_BYTES,
                scratch(&mut buffer),
            )
            .unwrap();
        assert!(input.next_frame().unwrap().is_some());
        assert!(input.next_frame().unwrap().is_some());
        assert!(input.next_frame().unwrap().is_none());
        assert!(matches!(
            input.finish(),
            Err(ActiveInputError::Stream(StreamError::Journal(
                ContainerError::Format(FormatError::InvalidValue)
            )))
        ));
        write(&root, &journal());
        let mut input = root
            .open_active_prefix(
                &Provider,
                bytes.selection(),
                bytes.view(2, 256),
                MAX_BYTES,
                scratch(&mut buffer),
            )
            .unwrap();
        assert!(input.next_frame().unwrap().is_some());
        write(&root, &[]);
        assert!(
            matches!(input.finish(),Err(ActiveInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidData)
        );
    }
}
