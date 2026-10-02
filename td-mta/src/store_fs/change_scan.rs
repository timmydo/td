//! Sequential selected-file work; returned changes still require external live-view validity.
use super::{ChangeFrameStep, ChangeInput, ChangeInputError, ChangeRoute, LockedRoot};
use crate::{
    change_cursor::{Cursor, Step},
    format::{bindings::Selection, table::MAX_RECORD_BYTES, ObjectType},
    frame_changes::Cell,
    ports::{ChangeCursor, ChangeStep, Crypto, Error, ViewIdentity},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeScanStep {
    /// Internal bounded work; the caller's change cursor has not advanced.
    Progress,
    Change(ChangeStep),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChangeScanRequest {
    pub view: ViewIdentity,
    pub after: ChangeCursor,
    pub kind: ObjectType,
    pub max_bytes: u64,
}
/// Holds one selected reader and one borrowed CHANGE arena across source transitions.
pub struct ChangeScan<'r, 'c, 'm, 'b, C: Crypto> {
    root: &'r LockedRoot,
    crypto: &'c C,
    selection: Selection<'m>,
    view: ViewIdentity,
    max_bytes: u64,
    cursor: Cursor,
    input: Option<ChangeInput<'r, 'c, 'm, 'b, C>>,
    cells: Option<&'b mut [Cell]>,
    frame_ready: bool,
    done: bool,
    failed: bool,
}
impl LockedRoot {
    /// Metadata admission only; the driver owns real pins and admits every work step.
    pub fn change_scan<'r, 'c, 'm, 'b, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'m>,
        request: ChangeScanRequest,
        cells: &'b mut [Cell],
    ) -> Result<ChangeScan<'r, 'c, 'm, 'b, C>, ChangeInputError> {
        let ChangeScanRequest {
            view,
            after,
            kind,
            max_bytes,
        } = request;
        ChangeRoute::new(selection, view)?;
        let cursor = Cursor::new(view, after, kind)?;
        Ok(ChangeScan {
            root: self,
            crypto,
            selection,
            view,
            max_bytes,
            cursor,
            input: None,
            cells: Some(cells),
            frame_ready: false,
            done: false,
            failed: false,
        })
    }
}
impl<'b, C: Crypto> ChangeScan<'_, '_, '_, 'b, C> {
    pub const fn after(&self) -> ChangeCursor {
        self.cursor.after()
    }
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    /// One open, frame read, frame drain, or source completion per call.
    pub fn advance(
        &mut self,
        view: ViewIdentity,
        after: ChangeCursor,
        scratch: &mut [u8; MAX_RECORD_BYTES],
    ) -> Result<ChangeScanStep, ChangeInputError> {
        if self.failed {
            return Err(ChangeInputError::Failed);
        }
        self.failed = true;
        let step = self.work(view, after, scratch)?;
        self.failed = false;
        Ok(step)
    }
    fn work(
        &mut self,
        view: ViewIdentity,
        after: ChangeCursor,
        scratch: &mut [u8; MAX_RECORD_BYTES],
    ) -> Result<ChangeScanStep, ChangeInputError> {
        // Validate identity and caller continuation before any file work.
        let needed = self.cursor.poll(view, after, None)?;
        if let Some(input) = self.input.as_mut() {
            if self.frame_ready {
                let frame = input.frame().ok_or(Error::Corrupt)?;
                let Step::Change(step) = self.cursor.poll(view, after, Some(frame))? else {
                    return Err(Error::Corrupt.into());
                };
                if matches!(step, ChangeStep::Complete) {
                    return Err(Error::Corrupt.into());
                }
                if matches!(step, ChangeStep::Advanced { .. }) {
                    self.frame_ready = false;
                }
                return Ok(ChangeScanStep::Change(step));
            }
            match input.advance(view, scratch)? {
                ChangeFrameStep::Locating { .. } => {}
                ChangeFrameStep::Frame { .. } => self.frame_ready = true,
                ChangeFrameStep::End => {
                    let input = self.input.take().ok_or(Error::Corrupt)?;
                    let (complete, cells) = input.finish()?;
                    drop(complete);
                    self.cells = Some(cells);
                }
            }
            return Ok(ChangeScanStep::Progress);
        }
        match needed {
            Step::Change(ChangeStep::Complete) => {
                self.done = true;
                Ok(ChangeScanStep::Change(ChangeStep::Complete))
            }
            Step::NeedFrame { sequence } => {
                let cells = self.cells.take().ok_or(Error::Corrupt)?;
                self.input = Some(self.root.open_changes_at(
                    self.crypto,
                    self.selection,
                    self.view,
                    sequence,
                    self.max_bytes,
                    cells,
                )?);
                Ok(ChangeScanStep::Progress)
            }
            _ => Err(Error::Corrupt.into()),
        }
    }
    /// Completion covers consumed sources only, never the full selected graph or pins.
    pub fn finish(self) -> Result<&'b mut [Cell], ChangeInputError> {
        if self.failed {
            return Err(ChangeInputError::Failed);
        }
        if !self.done || self.input.is_some() {
            return Err(Error::Invalid.into());
        }
        self.cells.ok_or_else(|| Error::Corrupt.into())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) fn probe(
    root: &LockedRoot,
    bytes: &super::active::ProbeBytes,
    scratch: &mut [u8; MAX_RECORD_BYTES],
) {
    use crate::format::Sequence;
    let mut cells = [Cell::EMPTY; 4];
    let pointer = cells.as_ptr();
    let view = bytes.view(2, 256);
    for (kind, expected) in [(ObjectType::Email, 1), (ObjectType::Thread, 0)] {
        let mut scan = root
            .change_scan(
                &td_crypto::Provider,
                bytes.selection(),
                ChangeScanRequest {
                    view,
                    after: ChangeCursor {
                        sequence: Sequence::from_u64(0),
                        operation: u32::MAX,
                    },
                    kind,
                    max_bytes: 277,
                },
                &mut cells,
            )
            .unwrap();
        let mut records = 0;
        let mut boundaries = 0;
        let mut complete = false;
        for _ in 0..20 {
            let after = scan.after();
            match scan.advance(view, after, scratch).unwrap() {
                ChangeScanStep::Progress => assert_eq!(scan.after(), after),
                ChangeScanStep::Change(ChangeStep::Record(_)) => records += 1,
                ChangeScanStep::Change(ChangeStep::Advanced { .. }) => boundaries += 1,
                ChangeScanStep::Change(ChangeStep::Complete) => {
                    complete = true;
                    break;
                }
            }
        }
        assert!(complete);
        assert_eq!((records, boundaries), (expected, 2));
        let cells = scan.finish().unwrap();
        assert_eq!(cells.len(), 4);
        assert_eq!(cells.as_ptr(), pointer);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::{active::fixture::*, tests::Fixture};
    use super::*;
    use crate::{
        format::{frame, Sequence},
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
    fn at(sequence: u64, operation: u32) -> ChangeCursor {
        ChangeCursor {
            sequence: Sequence::from_u64(sequence),
            operation,
        }
    }
    #[test]
    fn sources_transfer_and_empty_range_needs_no_file() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        probe(&root, &bytes, scratch.as_mut_slice().try_into().unwrap());
        let mut cells = [Cell::EMPTY; 1];
        let view = bytes.view(2, 256);
        write(&root, b"unread malformed bytes");
        let mut scan = root
            .change_scan(
                &Provider,
                bytes.selection(),
                ChangeScanRequest {
                    view,
                    after: at(2, u32::MAX),
                    kind: ObjectType::Email,
                    max_bytes: 0,
                },
                &mut cells,
            )
            .unwrap();
        assert_eq!(
            scan.advance(
                view,
                scan.after(),
                scratch.as_mut_slice().try_into().unwrap()
            )
            .unwrap(),
            ChangeScanStep::Change(ChangeStep::Complete)
        );
        assert_eq!(scan.finish().unwrap().len(), 1);
        assert!(std::mem::size_of::<ChangeScan<'_, '_, '_, '_, Provider>>() <= 8192);
    }
    #[test]
    fn partial_cursor_locates_without_advancing_and_drains_before_reading() {
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut data = journal();
        for sequence in 3..=5 {
            let mut next = hex(include_str!(
                "../../tests/fixtures/format-v1/frame-delete-change.hex"
            ));
            frame::seal(&Provider, Sequence::from_u64(sequence), 2, &mut next).unwrap();
            data.extend_from_slice(&next);
        }
        write(&root, &data);
        let mut view = bytes.view(5, data.len() as u64);
        view.history_floor = Sequence::from_u64(3);
        let mut cells = [Cell::EMPTY; 2];
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        let mut scan = root
            .change_scan(
                &Provider,
                bytes.selection(),
                ChangeScanRequest {
                    view,
                    after: at(4, 0),
                    kind: ObjectType::Email,
                    max_bytes: data.len() as u64,
                },
                &mut cells,
            )
            .unwrap();
        // Open, then locate frames 2, 3 and 4. No caller cursor progress.
        for _ in 0..4 {
            assert_eq!(
                scan.advance(view, at(4, 0), scratch.as_mut_slice().try_into().unwrap())
                    .unwrap(),
                ChangeScanStep::Progress
            );
            assert_eq!(scan.after(), at(4, 0));
        }
        assert!(
            matches!(scan.advance(view, at(4,0), scratch.as_mut_slice().try_into().unwrap()).unwrap(),
            ChangeScanStep::Change(ChangeStep::Record(record)) if record.cursor==at(4,1))
        );
        // Corrupt later bytes: already retained current changes/boundary remain usable.
        data[96 + 4 * 160 - 1] ^= 1;
        write(&root, &data);
        assert_eq!(
            scan.advance(view, at(4, 1), scratch.as_mut_slice().try_into().unwrap())
                .unwrap(),
            ChangeScanStep::Change(ChangeStep::Advanced {
                through: Sequence::from_u64(4)
            })
        );
        assert!(matches!(
            scan.advance(
                view,
                scan.after(),
                scratch.as_mut_slice().try_into().unwrap()
            ),
            Err(ChangeInputError::Input(_))
        ));
        assert!(scan.is_failed());
        assert!(matches!(scan.finish(), Err(ChangeInputError::Failed)));
    }
    #[test]
    fn selected_digest_failure_stops_before_next_source() {
        use crate::{
            format::{container::Error as ContainerError, journal_stream::Error as StreamError},
            store_paths::{Name, Number},
        };
        let fixture = Fixture::new();
        let (root, bytes) = setup(&fixture);
        let mut data = hex(include_str!(
            "../../tests/fixtures/format-v1/journal-with-frame.hex"
        ));
        data[96 + 64 + 12] ^= 1;
        frame::seal(&Provider, Sequence::from_u64(1), 1, &mut data[96..]).unwrap();
        let name = Name::account(ACCOUNT, AccountEntry::Journal(Number::new(1).unwrap())).unwrap();
        let mut path = [0; super::super::MAX_PATH_BYTES];
        std::fs::write(root.root.directory.join(&name, &mut path).unwrap(), &data).unwrap();
        // An attempted transition would fail opening this missing next file.
        let name = Name::account(ACCOUNT, AccountEntry::Journal(Number::new(2).unwrap())).unwrap();
        std::fs::remove_file(root.root.directory.join(&name, &mut path).unwrap()).unwrap();
        let view = bytes.view(2, 256);
        let mut cells = [Cell::EMPTY; 1];
        let mut scratch = vec![0; MAX_RECORD_BYTES];
        let mut scan = root
            .change_scan(
                &Provider,
                bytes.selection(),
                ChangeScanRequest {
                    view,
                    after: at(0, u32::MAX),
                    kind: ObjectType::Email,
                    max_bytes: 277,
                },
                &mut cells,
            )
            .unwrap();
        for _ in 0..2 {
            assert_eq!(
                scan.advance(
                    view,
                    scan.after(),
                    scratch.as_mut_slice().try_into().unwrap()
                )
                .unwrap(),
                ChangeScanStep::Progress
            );
        }
        assert_eq!(
            scan.advance(
                view,
                scan.after(),
                scratch.as_mut_slice().try_into().unwrap()
            )
            .unwrap(),
            ChangeScanStep::Change(ChangeStep::Advanced {
                through: Sequence::from_u64(1)
            })
        );
        assert!(matches!(
            scan.advance(
                view,
                scan.after(),
                scratch.as_mut_slice().try_into().unwrap()
            ),
            Err(ChangeInputError::Input(
                super::super::HistoryInputError::Stream(StreamError::Journal(
                    ContainerError::Checksum
                ))
            ))
        ));
        assert!(scan.is_failed());
        assert!(matches!(scan.finish(), Err(ChangeInputError::Failed)));
    }
    #[test]
    fn identity_continuation_and_io_errors_retire_entire_scan() {
        for mode in 0..4 {
            let fixture = Fixture::new();
            let (root, bytes) = setup(&fixture);
            let view = bytes.view(2, 256);
            let mut cells = [Cell::EMPTY; 1];
            let mut scratch = vec![0; MAX_RECORD_BYTES];
            let mut scan = root
                .change_scan(
                    &Provider,
                    bytes.selection(),
                    ChangeScanRequest {
                        view,
                        after: at(1, u32::MAX),
                        kind: ObjectType::Email,
                        max_bytes: if mode == 2 { 255 } else { 256 },
                    },
                    &mut cells,
                )
                .unwrap();
            if mode == 1 || mode == 3 {
                assert_eq!(
                    scan.advance(
                        view,
                        scan.after(),
                        scratch.as_mut_slice().try_into().unwrap()
                    )
                    .unwrap(),
                    ChangeScanStep::Progress
                );
                write(&root, &[]);
            }
            let mut supplied = view;
            if mode == 0 {
                supplied.generation += 1;
            }
            let after = if mode == 1 {
                at(2, u32::MAX)
            } else {
                scan.after()
            };
            let error = scan
                .advance(supplied, after, scratch.as_mut_slice().try_into().unwrap())
                .unwrap_err();
            match mode {
                0 => assert!(matches!(error, ChangeInputError::Policy(Error::Conflict))),
                1 => assert!(matches!(error, ChangeInputError::Policy(Error::Invalid))),
                _ => assert!(matches!(error, ChangeInputError::Input(_))),
            }
            assert!(scan.is_failed());
            assert!(matches!(
                scan.advance(
                    view,
                    scan.after(),
                    scratch.as_mut_slice().try_into().unwrap()
                ),
                Err(ChangeInputError::Failed)
            ));
            assert!(matches!(scan.finish(), Err(ChangeInputError::Failed)));
        }
    }
}
