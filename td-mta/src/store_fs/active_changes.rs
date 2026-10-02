//! Checked captured-prefix changes; actual runtime pins and cursor policy remain external.
use super::ActiveInputError;
use super::{change_input::ChangesInput, CompletePrefix, LockedRoot, PrefixReader};
use crate::{
    format::{bindings::Selection, journal_stream::Summary, table::MAX_RECORD_BYTES, Sequence},
    frame_changes::{Cell, CompleteChanges},
    ports::{Crypto, ViewIdentity},
};

pub struct ActiveChangesInput<'r, 'c, 'm, 'b, C: Crypto> {
    stream: ChangesInput<'c, 'b, C, PrefixReader<'r>>,
    selection: Selection<'m>,
    through: Sequence,
    bytes: u64,
}
impl LockedRoot {
    /// Caller holds actual prefix ownership/barriers; ViewIdentity is data, not a pin.
    /// This checks active fields only. History retention/floor validation is separate.
    pub fn open_active_changes<'r, 'c, 'm, 'b, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'m>,
        view: ViewIdentity,
        max_bytes: u64,
        cells: &'b mut [Cell],
    ) -> Result<ActiveChangesInput<'r, 'c, 'm, 'b, C>, ActiveInputError> {
        let (file, expected) = super::active::open_selected(self, selection, view, max_bytes)?;
        let stream = ChangesInput::new(crypto, file, expected, cells)?;
        Ok(ActiveChangesInput {
            stream,
            selection,
            through: view.committed_sequence,
            bytes: view.committed_offset,
        })
    }
}
impl<'r, 'b, C: Crypto> ActiveChangesInput<'r, '_, '_, 'b, C> {
    #[cfg(test)]
    pub(super) fn position(&self) -> u64 {
        self.stream.position()
    }
    pub fn is_failed(&self) -> bool {
        self.stream.is_failed()
    }
    /// Locally checked changes; captured completion and final-view proof remain pending.
    pub fn frame(&self) -> Option<&CompleteChanges<'_>> {
        self.stream.frame()
    }
    /// Discards prior changes; false means only the captured byte prefix is consumed.
    pub fn advance_frame(
        &mut self,
        scratch: &mut [u8; MAX_RECORD_BYTES],
    ) -> Result<bool, ActiveInputError> {
        self.stream.advance_frame(scratch)
    }
    pub fn finish(self) -> Result<CompleteActiveChanges<'r>, ActiveInputError> {
        let (complete, _) = self.finish_reuse()?;
        Ok(complete)
    }
    /// Checked completion plus the original full scratch capacity for another segment.
    pub fn finish_reuse(
        self,
    ) -> Result<(CompleteActiveChanges<'r>, &'b mut [Cell]), ActiveInputError> {
        let (file, summary, cells) = self.stream.finish_reuse()?;
        self.selection
            .check_active_prefix(self.through, self.bytes, summary)?;
        Ok((CompleteActiveChanges { file, summary }, cells))
    }
}
#[derive(Debug)]
pub struct CompleteActiveChanges<'r> {
    file: CompletePrefix<'r>,
    summary: Summary,
}
impl CompleteActiveChanges<'_> {
    pub fn file(&self) -> &CompletePrefix<'_> {
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
    bytes: &super::active::ProbeBytes,
    scratch: &mut [u8; MAX_RECORD_BYTES],
) {
    let mut cells = [Cell::EMPTY; 1];
    for (through, length, frames) in [(1, 96, 0), (2, 256, 1)] {
        let mut input = root
            .open_active_changes(
                &td_crypto::Provider,
                bytes.selection(),
                bytes.view(through, length),
                length,
                &mut cells,
            )
            .unwrap();
        assert!(input.frame().is_none());
        for _ in 0..frames {
            assert!(input.advance_frame(scratch).unwrap());
            let frame = input.frame().unwrap();
            let mut records = frame.records();
            let change = records.next().unwrap();
            assert_eq!(change.cursor.sequence.number(), 2);
            assert_eq!(change.cursor.operation, 1);
            assert!(records.next().is_none());
        }
        assert!(!input.advance_frame(scratch).unwrap());
        let complete = input.finish().unwrap();
        assert_eq!(complete.file().len(), length);
        assert_eq!(complete.summary().through().number(), through);
    }
    assert!(
        matches!(root.open_active_changes(&td_crypto::Provider, bytes.selection(), bytes.view(2,256),255,&mut cells),
        Err(ActiveInputError::Io(e)) if e.kind()==std::io::ErrorKind::InvalidInput)
    );
    let input = root
        .open_active_changes(
            &td_crypto::Provider,
            bytes.selection(),
            bytes.view(2, 256),
            256,
            &mut cells,
        )
        .unwrap();
    assert!(input.finish().is_err());
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::{active::fixture::*, tests::Fixture};
    use super::*;
    use crate::format::{Error as FormatError, JOURNAL_HEADER_BYTES, MAX_JOURNAL_FRAME_BYTES};
    use crate::{
        format::{container::Error as ContainerError, frame, journal_stream::Error as StreamError},
        store_paths::AccountEntry,
    };
    use std::{fs, io};
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
        let bytes = prepare(&root);
        (root, bytes)
    }
    #[test]
    fn captured_changes_ignore_later_complete_and_incomplete_appends() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        probe(&root, &bytes, scratch.as_mut_slice().try_into().unwrap());
        let mut journal = journal();
        let mut second = hex(include_str!(
            "../../tests/fixtures/format-v1/frame-delete-change.hex"
        ));
        frame::seal(&Provider, Sequence::from_u64(3), 2, &mut second).unwrap();
        journal.extend_from_slice(&second);
        write(&root, &journal);
        let mut cells = [Cell::EMPTY; 1];
        let mut input = root
            .open_active_changes(
                &Provider,
                bytes.selection(),
                bytes.view(2, 256),
                MAX_BYTES,
                &mut cells,
            )
            .unwrap();
        append(&root, b"unfinished append");
        assert!(input
            .advance_frame(scratch.as_mut_slice().try_into().unwrap())
            .unwrap());
        let record = input.frame().unwrap().records().next().unwrap();
        assert_eq!(record.cursor.sequence.number(), 2);
        assert!(!input
            .advance_frame(scratch.as_mut_slice().try_into().unwrap())
            .unwrap());
        assert_eq!(input.stream.position(), 256);
        let complete = input.finish().unwrap();
        assert_eq!(complete.file().len(), 256);
        assert_eq!(complete.file().read_at(256, &mut [0; 1]).unwrap(), 0);
        drop(complete);
        let mut input = root
            .open_active_changes(
                &Provider,
                bytes.selection(),
                bytes.view(3, journal.len() as u64),
                MAX_BYTES,
                &mut cells,
            )
            .unwrap();
        for sequence in [2, 3] {
            assert!(input
                .advance_frame(scratch.as_mut_slice().try_into().unwrap())
                .unwrap());
            assert_eq!(
                input
                    .frame()
                    .unwrap()
                    .records()
                    .next()
                    .unwrap()
                    .cursor
                    .sequence
                    .number(),
                sequence
            );
        }
        let complete = input.finish().unwrap();
        assert_eq!(complete.summary().through().number(), 3);
        assert_eq!(complete.file().len(), journal.len() as u64);
        assert!(std::mem::size_of::<ActiveChangesInput<'_, '_, '_, '_, Provider>>() <= 8192);
    }
    #[test]
    fn active_fields_and_final_captured_sequence_are_checked() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        let mut cells = [Cell::EMPTY; 1];
        let name = crate::store_paths::Name::account(
            ACCOUNT,
            AccountEntry::Journal(crate::store_paths::Number::new(2).unwrap()),
        )
        .unwrap();
        let mut path = [0; super::super::MAX_PATH_BYTES];
        fs::remove_file(root.root.directory.join(&name, &mut path).unwrap()).unwrap();
        for mode in 0..8 {
            let mut view = bytes.view(2, 256);
            match mode {
                0 => view.account = crate::ids::AccountId::from_bytes([1; 16]),
                1 => view.epoch = crate::ids::StoreEpoch::from_bytes([1; 16]),
                2 => view.generation += 1,
                3 => view.checkpoint = Sequence::default(),
                4 => view.segment += 1,
                5 => view.committed_offset = 95,
                6 => view.committed_sequence = Sequence::default(),
                _ => view.committed_offset = MAX_BYTES + 1,
            }
            let expected = if mode == 7 {
                FormatError::Limit
            } else {
                FormatError::InvalidValue
            };
            assert!(
                matches!(root.open_active_changes(&Provider, bytes.selection(), view, MAX_BYTES, &mut cells),
                Err(ActiveInputError::Stream(StreamError::Journal(ContainerError::Format(error)))) if error == expected),
                "mode {mode}"
            );
        }
        assert!(
            matches!(root.open_active_changes(&Provider,bytes.selection(),bytes.view(2,256),255,&mut cells),
            Err(ActiveInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidInput)
        );
        assert!(
            matches!(root.open_active_changes(&Provider,bytes.selection(),bytes.view(2,256),256,&mut cells),
            Err(ActiveInputError::Io(e)) if e.kind()==io::ErrorKind::NotFound)
        );
        let mut journal = journal();
        let mut second = hex(include_str!(
            "../../tests/fixtures/format-v1/frame-delete-change.hex"
        ));
        frame::seal(&Provider, Sequence::from_u64(3), 2, &mut second).unwrap();
        journal.extend_from_slice(&second);
        write(&root, &journal);
        // Two frames fit the broad one-frame length bounds, but cannot finish at sequence two.
        let mut input = root
            .open_active_changes(
                &Provider,
                bytes.selection(),
                bytes.view(2, journal.len() as u64),
                MAX_BYTES,
                &mut cells,
            )
            .unwrap();
        assert!(input
            .advance_frame(scratch.as_mut_slice().try_into().unwrap())
            .unwrap());
        assert!(input
            .advance_frame(scratch.as_mut_slice().try_into().unwrap())
            .unwrap());
        assert!(matches!(
            input.finish(),
            Err(ActiveInputError::Stream(StreamError::Journal(
                ContainerError::Format(FormatError::InvalidValue)
            )))
        ));
    }
    #[test]
    fn failures_and_partial_captured_frames_never_complete() {
        for mode in 0..5 {
            let fixture = Fixture::new();
            let (root, bytes) = setup(&fixture);
            let mut scratch = vec![0; MAX_RECORD_BYTES];
            let mut cells = [Cell::EMPTY; 1];
            let mut input = root
                .open_active_changes(
                    &Provider,
                    bytes.selection(),
                    bytes.view(2, if mode == 0 { 255 } else { 256 }),
                    MAX_BYTES,
                    if mode == 1 { &mut [] } else { &mut cells },
                )
                .unwrap();
            if mode == 2 {
                write(&root, &journal()[..250]);
            }
            if mode == 3 {
                assert!(
                    matches!(input.stream.advance_using(scratch.as_mut_slice().try_into().unwrap(), |_,_|Err(io::ErrorKind::Interrupted.into())),Err(ActiveInputError::Io(e)) if e.kind()==io::ErrorKind::Interrupted)
                );
            } else if mode == 4 {
                assert!(input
                    .advance_frame(scratch.as_mut_slice().try_into().unwrap())
                    .unwrap());
                write(&root, &journal()[..250]);
                assert!(
                    matches!(input.finish(),Err(ActiveInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidData)
                );
                continue;
            } else {
                let error = input
                    .advance_frame(scratch.as_mut_slice().try_into().unwrap())
                    .unwrap_err();
                match mode {
                    0 => assert!(matches!(
                        error,
                        ActiveInputError::Stream(StreamError::Frame(
                            crate::format::frame::DecodeError::Invalid(ContainerError::Format(
                                FormatError::InvalidValue
                            ))
                        ))
                    )),
                    1 => assert!(matches!(
                        error,
                        ActiveInputError::Stream(StreamError::Journal(ContainerError::Format(
                            FormatError::OutputFull
                        )))
                    )),
                    _ => assert!(
                        matches!(error, ActiveInputError::Io(e) if e.kind()==io::ErrorKind::UnexpectedEof)
                    ),
                }
            }
            assert!(input.is_failed());
            assert!(input.frame().is_none());
            assert!(
                matches!(input.advance_frame(scratch.as_mut_slice().try_into().unwrap()),Err(ActiveInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
            );
            assert!(
                matches!(input.finish(),Err(ActiveInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
            );
        }
    }
}
