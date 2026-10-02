//! One bounded frame per physical location step; selected completion remains explicit.
use super::{
    ActiveChangesInput, ChangeRoute, ChangeSource, CompleteActiveChanges, CompleteHistoryChanges,
    HistoryChangesInput, HistoryInputError, LockedRoot,
};
use crate::{
    format::{bindings::Selection, table::MAX_RECORD_BYTES, Sequence},
    frame_changes::{Cell, CompleteChanges},
    ports::{Crypto, Error as PolicyError, ViewIdentity},
};

#[derive(Debug)]
pub enum ChangeInputError {
    Policy(PolicyError),
    Input(HistoryInputError),
    Failed,
}
impl From<PolicyError> for ChangeInputError {
    fn from(error: PolicyError) -> Self {
        Self::Policy(error)
    }
}
impl From<HistoryInputError> for ChangeInputError {
    fn from(error: HistoryInputError) -> Self {
        Self::Input(error)
    }
}
impl std::fmt::Display for ChangeInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Policy(error) => write!(f, "change input policy: {error}"),
            Self::Input(error) => write!(f, "change input: {error}"),
            Self::Failed => f.write_str("change input retired"),
        }
    }
}
impl std::error::Error for ChangeInputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Policy(error) => Some(error),
            Self::Input(error) => Some(error),
            Self::Failed => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeFrameStep {
    Locating { through: Sequence },
    Frame { sequence: Sequence },
    End,
}
enum Reader<'r, 'c, 'm, 'b, C: Crypto> {
    History(HistoryChangesInput<'r, 'c, 'm, 'b, C>),
    Active(ActiveChangesInput<'r, 'c, 'm, 'b, C>),
}
impl<C: Crypto> Reader<'_, '_, '_, '_, C> {
    fn advance(&mut self, scratch: &mut [u8; MAX_RECORD_BYTES]) -> Result<bool, HistoryInputError> {
        match self {
            Self::History(input) => input.advance_frame(scratch),
            Self::Active(input) => input.advance_frame(scratch),
        }
    }
    fn frame(&self) -> Option<&CompleteChanges<'_>> {
        match self {
            Self::History(input) => input.frame(),
            Self::Active(input) => input.frame(),
        }
    }
}
pub struct ChangeInput<'r, 'c, 'm, 'b, C: Crypto> {
    reader: Reader<'r, 'c, 'm, 'b, C>,
    view: ViewIdentity,
    source: ChangeSource,
    target: Sequence,
    through: Sequence,
    seen: Sequence,
    reached: bool,
    failed: bool,
}
#[derive(Debug)]
pub enum ChangeCompletion<'r> {
    History(CompleteHistoryChanges<'r>),
    Active(CompleteActiveChanges<'r>),
}
impl LockedRoot {
    /// Caller admits bounded frame work and owns actual immutable view/recovery pins.
    pub fn open_changes_at<'r, 'c, 'm, 'b, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'m>,
        view: ViewIdentity,
        target: Sequence,
        max_bytes: u64,
        cells: &'b mut [Cell],
    ) -> Result<ChangeInput<'r, 'c, 'm, 'b, C>, ChangeInputError> {
        let route = ChangeRoute::new(selection, view)?;
        let source = route.source(view, target)?;
        let (reader, base, through) = match source {
            ChangeSource::History { index } => {
                let descriptor = selection
                    .manifest()
                    .history(index)
                    .map_err(|_| PolicyError::Corrupt)?;
                (
                    Reader::History(
                        self.open_history_changes(crypto, selection, index, max_bytes, cells)?,
                    ),
                    descriptor.base,
                    descriptor.through,
                )
            }
            ChangeSource::Active => (
                Reader::Active(
                    self.open_active_changes(crypto, selection, view, max_bytes, cells)?,
                ),
                view.checkpoint,
                view.committed_sequence,
            ),
        };
        Ok(ChangeInput {
            reader,
            view,
            source,
            target,
            through,
            seen: base,
            reached: false,
            failed: false,
        })
    }
}
impl<'r, 'b, C: Crypto> ChangeInput<'r, '_, '_, 'b, C> {
    pub const fn identity(&self) -> ViewIdentity {
        self.view
    }
    pub const fn source(&self) -> ChangeSource {
        self.source
    }
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    /// Hidden while locating or retired; all returned changes remain provisional.
    pub fn frame(&self) -> Option<&CompleteChanges<'_>> {
        if self.reached && !self.failed {
            self.reader.frame()
        } else {
            None
        }
    }
    /// Discards the prior frame, reads at most one bounded frame and returns to the driver.
    pub fn advance(
        &mut self,
        view: ViewIdentity,
        scratch: &mut [u8; MAX_RECORD_BYTES],
    ) -> Result<ChangeFrameStep, ChangeInputError> {
        if self.failed {
            return Err(ChangeInputError::Failed);
        }
        self.failed = true;
        if view != self.view {
            return Err(PolicyError::Conflict.into());
        }
        let step = if self.reader.advance(scratch)? {
            let frame = self.reader.frame().ok_or(PolicyError::Corrupt)?;
            let sequence = frame.summary().header().sequence;
            if sequence > self.through || (!self.reached && sequence > self.target) {
                return Err(PolicyError::Corrupt.into());
            }
            self.seen = sequence;
            if sequence == self.target {
                self.reached = true;
            }
            if self.reached {
                ChangeFrameStep::Frame { sequence }
            } else {
                ChangeFrameStep::Locating { through: sequence }
            }
        } else {
            if !self.reached || self.seen != self.through {
                return Err(PolicyError::Corrupt.into());
            }
            ChangeFrameStep::End
        };
        self.failed = false;
        Ok(step)
    }
    pub fn finish(self) -> Result<(ChangeCompletion<'r>, &'b mut [Cell]), ChangeInputError> {
        if self.failed {
            return Err(ChangeInputError::Failed);
        }
        if !self.reached {
            return Err(PolicyError::Invalid.into());
        }
        match self.reader {
            Reader::History(input) => {
                let (complete, cells) = input.finish_reuse()?;
                Ok((ChangeCompletion::History(complete), cells))
            }
            Reader::Active(input) => {
                let (complete, cells) = input.finish_reuse()?;
                Ok((ChangeCompletion::Active(complete), cells))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) fn probe(
    root: &LockedRoot,
    bytes: &super::active::ProbeBytes,
    scratch: &mut [u8; MAX_RECORD_BYTES],
) {
    let mut storage = [Cell::EMPTY; 4];
    let pointer = storage.as_ptr();
    let view = bytes.view(2, 256);
    let mut history = root
        .open_changes_at(
            &td_crypto::Provider,
            bytes.selection(),
            view,
            Sequence::from_u64(1),
            277,
            &mut storage,
        )
        .unwrap();
    assert!(history.frame().is_none());
    assert_eq!(
        history.advance(view, scratch).unwrap(),
        ChangeFrameStep::Frame {
            sequence: Sequence::from_u64(1)
        }
    );
    assert!(history.frame().unwrap().is_empty());
    let (complete, cells) = history.finish().unwrap();
    assert!(matches!(complete, ChangeCompletion::History(_)));
    drop(complete);
    assert_eq!(cells.len(), 4);
    assert_eq!(cells.as_ptr(), pointer);
    let mut active = root
        .open_changes_at(
            &td_crypto::Provider,
            bytes.selection(),
            view,
            Sequence::from_u64(2),
            256,
            cells,
        )
        .unwrap();
    assert_eq!(
        active.advance(view, scratch).unwrap(),
        ChangeFrameStep::Frame {
            sequence: Sequence::from_u64(2)
        }
    );
    scratch.fill(0xa5);
    assert_eq!(
        active
            .frame()
            .unwrap()
            .records()
            .next()
            .unwrap()
            .cursor
            .operation,
        1
    );
    assert_eq!(active.advance(view, scratch).unwrap(), ChangeFrameStep::End);
    let (complete, cells) = active.finish().unwrap();
    assert!(matches!(complete, ChangeCompletion::Active(_)));
    drop(complete);
    assert_eq!(cells.len(), 4);
    assert_eq!(cells.as_ptr(), pointer);
    let empty = root
        .open_active_changes(
            &td_crypto::Provider,
            bytes.selection(),
            bytes.view(1, 96),
            96,
            cells,
        )
        .unwrap();
    let (complete, cells) = empty.finish_reuse().unwrap();
    drop(complete);
    assert_eq!(cells.len(), 4);
    assert_eq!(cells.as_ptr(), pointer);
    assert!(matches!(
        root.open_changes_at(
            &td_crypto::Provider,
            bytes.selection(),
            view,
            Sequence::from_u64(3),
            256,
            cells
        ),
        Err(ChangeInputError::Policy(PolicyError::Invalid))
    ));
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::{active::fixture::*, tests::Fixture};
    use super::*;
    use crate::{
        format::{container::Error as ContainerError, frame, journal_stream::Error as StreamError},
        store_paths::AccountEntry,
    };
    use td_crypto::Provider;
    fn setup(fixture: &Fixture) -> (LockedRoot, Bytes) {
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        for entry in [AccountEntry::Root, AccountEntry::Metadata] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
        super::super::history::prepare_probe(&root);
        let bytes = prepare(&root);
        (root, bytes)
    }
    fn four_frames() -> Vec<u8> {
        let mut data = journal();
        for sequence in 3..=5 {
            let mut next = hex(include_str!(
                "../../tests/fixtures/format-v1/frame-delete-change.hex"
            ));
            frame::seal(&Provider, Sequence::from_u64(sequence), 2, &mut next).unwrap();
            data.extend_from_slice(&next);
        }
        data
    }
    #[test]
    fn locates_one_frame_per_step_then_keeps_streaming_and_reclaims_slots() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        probe(&root, &bytes, scratch.as_mut_slice().try_into().unwrap());
        let data = four_frames();
        write(&root, &data);
        let mut view = bytes.view(5, data.len() as u64);
        view.history_floor = Sequence::from_u64(3);
        let mut cells = [Cell::EMPTY; 4];
        let pointer = cells.as_ptr();
        let mut input = root
            .open_changes_at(
                &Provider,
                bytes.selection(),
                view,
                Sequence::from_u64(4),
                data.len() as u64,
                &mut cells,
            )
            .unwrap();
        assert_eq!(input.identity(), view);
        assert_eq!(input.source(), ChangeSource::Active);
        for sequence in 2..=5 {
            let step = input
                .advance(view, scratch.as_mut_slice().try_into().unwrap())
                .unwrap();
            let expected = if sequence < 4 {
                ChangeFrameStep::Locating {
                    through: Sequence::from_u64(sequence),
                }
            } else {
                ChangeFrameStep::Frame {
                    sequence: Sequence::from_u64(sequence),
                }
            };
            assert_eq!(step, expected);
            let position = match &input.reader {
                Reader::Active(input) => input.position(),
                Reader::History(input) => input.position(),
            };
            assert_eq!(position, 96 + (sequence - 1) * 160);
            if sequence < 4 {
                assert!(input.frame().is_none());
            } else {
                assert_eq!(
                    input.frame().unwrap().summary().header().sequence.number(),
                    sequence
                );
            }
        }
        append(&root, b"later incomplete append");
        assert_eq!(
            input
                .advance(view, scratch.as_mut_slice().try_into().unwrap())
                .unwrap(),
            ChangeFrameStep::End
        );
        assert!(input.frame().is_none());
        let (complete, cells) = input.finish().unwrap();
        assert!(matches!(complete, ChangeCompletion::Active(_)));
        assert_eq!(cells.len(), 4);
        assert_eq!(cells.as_ptr(), pointer);
        assert!(std::mem::size_of::<ChangeInput<'_, '_, '_, '_, Provider>>() <= 8192);
    }
    #[test]
    fn changed_view_corrupt_location_and_premature_finish_refuse() {
        for mode in 0..4 {
            let fixture = Fixture::new();
            let (root, bytes) = setup(&fixture);
            let mut data = four_frames();
            write(&root, &data);
            let view = bytes.view(5, data.len() as u64);
            let mut scratch = vec![0; MAX_RECORD_BYTES];
            let mut cells = [Cell::EMPTY; 1];
            let mut input = root
                .open_changes_at(
                    &Provider,
                    bytes.selection(),
                    view,
                    Sequence::from_u64(4),
                    data.len() as u64,
                    &mut cells,
                )
                .unwrap();
            assert_eq!(
                input
                    .advance(view, scratch.as_mut_slice().try_into().unwrap())
                    .unwrap(),
                ChangeFrameStep::Locating {
                    through: Sequence::from_u64(2)
                }
            );
            if mode == 0 {
                assert!(matches!(
                    input.finish(),
                    Err(ChangeInputError::Policy(PolicyError::Invalid))
                ));
                continue;
            }
            let mut supplied = view;
            if mode == 1 {
                supplied.generation += 1;
            }
            if mode == 2 {
                data[96 + 2 * 160 - 1] ^= 1;
                write(&root, &data);
            }
            if mode == 3 {
                data.truncate(256);
                write(&root, &data);
            }
            let error = input
                .advance(supplied, scratch.as_mut_slice().try_into().unwrap())
                .unwrap_err();
            match mode {
                1 => assert!(matches!(
                    error,
                    ChangeInputError::Policy(PolicyError::Conflict)
                )),
                2 => assert!(matches!(
                    error,
                    ChangeInputError::Input(HistoryInputError::Stream(StreamError::Frame(
                        crate::format::frame::DecodeError::Invalid(ContainerError::Checksum)
                    )))
                )),
                _ => assert!(
                    matches!(error,ChangeInputError::Input(HistoryInputError::Io(e)) if e.kind()==std::io::ErrorKind::UnexpectedEof)
                ),
            }
            assert!(input.is_failed());
            assert!(input.frame().is_none());
            assert!(matches!(
                input.advance(view, scratch.as_mut_slice().try_into().unwrap()),
                Err(ChangeInputError::Failed)
            ));
            assert!(matches!(input.finish(), Err(ChangeInputError::Failed)));
        }
    }
    #[test]
    fn short_sequence_range_cannot_report_end_or_completion() {
        use crate::{format::operation::Operation, format::ObjectType, ports::ChangeAction};
        use std::error::Error as _;
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut data = journal();
        data.truncate(96);
        let mut frame_bytes = vec![0; 104 + 6 * 28];
        for output in frame_bytes[64..64 + 6 * 28].as_chunks_mut::<28>().0 {
            Operation::change(ObjectType::Email, ChangeAction::Updated, &[1; 16])
                .encode(output)
                .unwrap();
        }
        frame::seal(&Provider, Sequence::from_u64(2), 6, &mut frame_bytes).unwrap();
        data.extend_from_slice(&frame_bytes);
        write(&root, &data);
        let view = bytes.view(3, data.len() as u64);
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        let mut cells = [Cell::EMPTY; 6];
        for target in [2, 3] {
            let mut input = root
                .open_changes_at(
                    &Provider,
                    bytes.selection(),
                    view,
                    Sequence::from_u64(target),
                    data.len() as u64,
                    &mut cells,
                )
                .unwrap();
            let step = input
                .advance(view, scratch.as_mut_slice().try_into().unwrap())
                .unwrap();
            assert_eq!(
                step,
                if target == 2 {
                    ChangeFrameStep::Frame {
                        sequence: Sequence::from_u64(2),
                    }
                } else {
                    ChangeFrameStep::Locating {
                        through: Sequence::from_u64(2),
                    }
                }
            );
            let error = input
                .advance(view, scratch.as_mut_slice().try_into().unwrap())
                .unwrap_err();
            assert!(matches!(
                error,
                ChangeInputError::Policy(PolicyError::Corrupt)
            ));
            assert_eq!(
                error.source().unwrap().downcast_ref::<PolicyError>(),
                Some(&PolicyError::Corrupt)
            );
            assert!(input.frame().is_none());
            assert!(input.is_failed());
            assert!(matches!(input.finish(), Err(ChangeInputError::Failed)));
        }
        let error = ChangeInputError::Input(HistoryInputError::Io(
            std::io::ErrorKind::UnexpectedEof.into(),
        ));
        assert_eq!(
            error
                .source()
                .unwrap()
                .source()
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        assert!(ChangeInputError::Failed.source().is_none());
    }
    #[test]
    fn captured_sequence_limit_hides_excess_frames_and_finish_checks_extent() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let data = four_frames();
        write(&root, &data);
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        let mut cells = [Cell::EMPTY; 1];
        let view = bytes.view(2, data.len() as u64);
        let mut input = root
            .open_changes_at(
                &Provider,
                bytes.selection(),
                view,
                Sequence::from_u64(2),
                data.len() as u64,
                &mut cells,
            )
            .unwrap();
        assert!(matches!(
            input
                .advance(view, scratch.as_mut_slice().try_into().unwrap())
                .unwrap(),
            ChangeFrameStep::Frame { .. }
        ));
        assert!(matches!(
            input.advance(view, scratch.as_mut_slice().try_into().unwrap()),
            Err(ChangeInputError::Policy(PolicyError::Corrupt))
        ));
        assert!(input.frame().is_none());
        assert!(matches!(input.finish(), Err(ChangeInputError::Failed)));
        let view = bytes.view(5, data.len() as u64);
        let mut input = root
            .open_changes_at(
                &Provider,
                bytes.selection(),
                view,
                Sequence::from_u64(2),
                data.len() as u64,
                &mut cells,
            )
            .unwrap();
        input
            .advance(view, scratch.as_mut_slice().try_into().unwrap())
            .unwrap();
        assert!(
            matches!(input.finish(),Err(ChangeInputError::Input(HistoryInputError::Io(e))) if e.kind()==std::io::ErrorKind::InvalidInput)
        );
    }
}
