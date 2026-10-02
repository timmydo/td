//! Complete every retained selected segment using shared change slots and record scratch.
use super::super::{HistoryChangesInput, HistoryInputError, LockedRoot};
use crate::{
    format::{self, bindings::Selection, container::Current, table::MAX_RECORD_BYTES, Sequence},
    frame_changes::Cell,
    ports::Crypto,
};

#[derive(Debug)]
pub enum HistorySweepError {
    Input(HistoryInputError),
    Format(format::Error),
    ByteLimit,
    FrameLimit,
    ChangeCapacity,
    Failed,
    Incomplete,
}
impl std::fmt::Display for HistorySweepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "selected history sweep: {self:?}")
    }
}
impl std::error::Error for HistorySweepError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Input(e) => Some(e),
            Self::Format(e) => Some(e),
            _ => None,
        }
    }
}
fn input_error(error: HistoryInputError) -> HistorySweepError {
    match error {
        HistoryInputError::Stream(format::journal_stream::Error::Journal(
            format::container::Error::Format(format::Error::OutputFull),
        )) => HistorySweepError::ChangeCapacity,
        other => HistorySweepError::Input(other),
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistorySweepLimits {
    pub bytes: u64,
    pub frames: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistorySweepStep {
    Opened { segment: u64 },
    Frame { segment: u64, sequence: Sequence },
    Exhausted { segment: u64 },
    Verified { segment: u64 },
    Complete,
}
pub struct HistorySweep<'r, 'c, 'm, 'b, C: Crypto> {
    root: &'r LockedRoot,
    crypto: &'c C,
    selection: Selection<'m>,
    cells: Option<&'b mut [Cell]>,
    input: Option<HistoryChangesInput<'r, 'c, 'm, 'b, C>>,
    index: usize,
    finish_input: bool,
    bytes: u64,
    expected_frames: u64,
    frames: u64,
    complete: bool,
    failed: bool,
}
impl<'r, 'c, 'm, 'b, C: Crypto> HistorySweep<'r, 'c, 'm, 'b, C> {
    /// Caller holds immutable selected-file ownership or actual stopped-store exclusion.
    pub fn new(
        root: &'r LockedRoot,
        crypto: &'c C,
        selection: Selection<'m>,
        cells: &'b mut [Cell],
        limits: HistorySweepLimits,
    ) -> Result<Self, HistorySweepError> {
        let mut bytes = 0u64;
        let mut frames = 0u64;
        for index in 0..selection.manifest().history_count() {
            let descriptor = selection
                .manifest()
                .history(index)
                .map_err(HistorySweepError::Format)?;
            bytes = bytes
                .checked_add(descriptor.file_bytes)
                .ok_or(HistorySweepError::Format(format::Error::Overflow))?;
            let count = descriptor
                .through
                .number()
                .checked_sub(descriptor.base.number())
                .ok_or(HistorySweepError::Format(format::Error::InvalidValue))?;
            frames = frames
                .checked_add(count)
                .ok_or(HistorySweepError::Format(format::Error::Overflow))?;
        }
        if bytes > limits.bytes {
            return Err(HistorySweepError::ByteLimit);
        }
        if frames > limits.frames {
            return Err(HistorySweepError::FrameLimit);
        }
        Ok(Self {
            root,
            crypto,
            selection,
            cells: Some(cells),
            input: None,
            index: 0,
            finish_input: false,
            bytes,
            expected_frames: frames,
            frames: 0,
            complete: false,
            failed: false,
        })
    }
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    pub const fn is_complete(&self) -> bool {
        self.complete && !self.failed
    }
    /// One selected open, bounded complete frame, or segment completion per advance.
    pub fn advance(
        &mut self,
        scratch: &mut [u8; MAX_RECORD_BYTES],
    ) -> Result<HistorySweepStep, HistorySweepError> {
        if self.failed {
            return Err(HistorySweepError::Failed);
        }
        self.failed = true;
        let step = self.work(scratch)?;
        self.failed = false;
        Ok(step)
    }
    fn work(
        &mut self,
        scratch: &mut [u8; MAX_RECORD_BYTES],
    ) -> Result<HistorySweepStep, HistorySweepError> {
        if self.complete {
            return Ok(HistorySweepStep::Complete);
        }
        if self.index == self.selection.manifest().history_count() {
            if self.frames != self.expected_frames {
                return Err(HistorySweepError::Format(format::Error::InvalidValue));
            }
            self.complete = true;
            return Ok(HistorySweepStep::Complete);
        }
        let descriptor = self
            .selection
            .manifest()
            .history(self.index)
            .map_err(HistorySweepError::Format)?;
        if self.finish_input {
            let input = self.input.take().ok_or(HistorySweepError::Incomplete)?;
            let (complete, cells) = input.finish_reuse().map_err(input_error)?;
            drop(complete);
            self.cells = Some(cells);
            self.finish_input = false;
            self.index = self
                .index
                .checked_add(1)
                .ok_or(HistorySweepError::Format(format::Error::Overflow))?;
            return Ok(HistorySweepStep::Verified {
                segment: descriptor.segment,
            });
        }
        if let Some(input) = self.input.as_mut() {
            if input
                .frame()
                .is_some_and(|frame| frame.summary().header().sequence == descriptor.through)
            {
                self.finish_input = true;
                return Ok(HistorySweepStep::Exhausted {
                    segment: descriptor.segment,
                });
            }
            if input.advance_frame(scratch).map_err(input_error)? {
                let sequence = input
                    .frame()
                    .ok_or(HistorySweepError::Incomplete)?
                    .summary()
                    .header()
                    .sequence;
                if sequence > descriptor.through {
                    return Err(HistorySweepError::Format(format::Error::InvalidValue));
                }
                self.frames = self
                    .frames
                    .checked_add(1)
                    .ok_or(HistorySweepError::Format(format::Error::Overflow))?;
                return Ok(HistorySweepStep::Frame {
                    segment: descriptor.segment,
                    sequence,
                });
            }
            self.finish_input = true;
            return Ok(HistorySweepStep::Exhausted {
                segment: descriptor.segment,
            });
        }
        let cells = self.cells.take().ok_or(HistorySweepError::Incomplete)?;
        self.input = Some(
            self.root
                .open_history_changes(
                    self.crypto,
                    self.selection,
                    self.index,
                    descriptor.file_bytes,
                    cells,
                )
                .map_err(input_error)?,
        );
        Ok(HistorySweepStep::Opened {
            segment: descriptor.segment,
        })
    }
    pub fn finish(self) -> Result<(CompleteHistorySweep, &'b mut [Cell]), HistorySweepError> {
        if self.failed {
            return Err(HistorySweepError::Failed);
        }
        if !self.complete {
            return Err(HistorySweepError::Incomplete);
        }
        let cells = self.cells.ok_or(HistorySweepError::Incomplete)?;
        Ok((
            CompleteHistorySweep {
                current: self.selection.current(),
                checkpoint: self.selection.manifest().header().through,
                segments: self.index,
                frames: self.frames,
                bytes: self.bytes,
            },
            cells,
        ))
    }
}
/// Selected immutable history only; active prefix, final rows and pins remain separate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteHistorySweep {
    current: Current,
    checkpoint: Sequence,
    segments: usize,
    frames: u64,
    bytes: u64,
}
impl CompleteHistorySweep {
    pub const fn current(self) -> Current {
        self.current
    }
    pub const fn checkpoint(self) -> Sequence {
        self.checkpoint
    }
    pub const fn segments(self) -> usize {
        self.segments
    }
    pub const fn frames(self) -> u64 {
        self.frames
    }
    pub const fn bytes(self) -> u64 {
        self.bytes
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) fn probe(
    root: &LockedRoot,
    bytes: &super::ProbeBytes,
    scratch: &mut [u8; MAX_RECORD_BYTES],
) {
    let mut cells = [Cell::EMPTY; 2];
    let original = cells.as_ptr();
    let selection = bytes.selection();
    let mut sweep = HistorySweep::new(
        root,
        &td_crypto::Provider,
        selection,
        &mut cells,
        HistorySweepLimits {
            bytes: 277,
            frames: 1,
        },
    )
    .unwrap();
    assert_eq!(
        sweep.advance(scratch).unwrap(),
        HistorySweepStep::Opened { segment: 1 }
    );
    assert_eq!(
        sweep.advance(scratch).unwrap(),
        HistorySweepStep::Frame {
            segment: 1,
            sequence: Sequence::from_u64(1)
        }
    );
    assert_eq!(
        sweep.advance(scratch).unwrap(),
        HistorySweepStep::Exhausted { segment: 1 }
    );
    assert_eq!(
        sweep.advance(scratch).unwrap(),
        HistorySweepStep::Verified { segment: 1 }
    );
    assert_eq!(sweep.advance(scratch).unwrap(), HistorySweepStep::Complete);
    assert_eq!(sweep.advance(scratch).unwrap(), HistorySweepStep::Complete);
    let (done, cells) = sweep.finish().unwrap();
    assert_eq!((cells.as_ptr(), cells.len()), (original, 2));
    assert_eq!(done.current(), selection.current());
    assert_eq!(done.checkpoint(), selection.manifest().header().through);
    assert_eq!((done.segments(), done.frames(), done.bytes()), (1, 1, 277));
    assert!(matches!(
        HistorySweep::new(
            root,
            &td_crypto::Provider,
            selection,
            cells,
            HistorySweepLimits {
                bytes: 276,
                frames: 1
            }
        ),
        Err(HistorySweepError::ByteLimit)
    ));
    assert!(matches!(
        HistorySweep::new(
            root,
            &td_crypto::Provider,
            selection,
            cells,
            HistorySweepLimits {
                bytes: 277,
                frames: 0
            }
        ),
        Err(HistorySweepError::FrameLimit)
    ));
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::super::super::tests::Fixture;
    use super::super::fixture::{self, Bytes, ACCOUNT};
    use super::*;
    use crate::{
        format::{
            container::JournalHeader, frame, manifest, operation::Operation, ObjectType, Table,
            FRAME_FOOTER_BYTES, FRAME_HEADER_BYTES, JOURNAL_HEADER_BYTES, TABLE_COUNT,
        },
        ports::{ChangeAction, Digest},
        store_paths::{AccountEntry, Name, Number},
    };
    use std::{fs, os::unix::fs::PermissionsExt};
    use td_crypto::Provider;
    fn hash(bytes: &[u8]) -> [u8; 32] {
        let mut hash = Provider.sha256().unwrap();
        hash.update(bytes).unwrap();
        hash.finish().unwrap()
    }
    fn setup(f: &Fixture) -> (LockedRoot, Bytes) {
        let root = f.locked();
        root.create_accounts_directory().unwrap();
        root.create_account_directory(ACCOUNT, AccountEntry::Root)
            .unwrap();
        root.create_account_directory(ACCOUNT, AccountEntry::Metadata)
            .unwrap();
        let bytes = fixture::prepare(&root);
        (root, bytes)
    }
    fn path(f: &Fixture, segment: u64) -> std::path::PathBuf {
        f.path.join(
            Name::account(
                ACCOUNT,
                AccountEntry::Journal(Number::new(segment).unwrap()),
            )
            .unwrap()
            .as_path()
            .unwrap(),
        )
    }
    fn segments(f: &Fixture, bytes: &mut Bytes, count: usize) -> u64 {
        let selection = bytes.selection();
        let manifest = selection.manifest();
        let current = selection.current();
        let mut history = Vec::new();
        let mut total = 0;
        for index in 0..count {
            let segment = index as u64 + 1;
            let mut journal = vec![0; JOURNAL_HEADER_BYTES];
            JournalHeader {
                account: ACCOUNT,
                epoch: selection.store().epoch,
                segment,
                base: Sequence::from_u64(index as u64),
            }
            .encode(&Provider, &mut journal)
            .unwrap();
            let mut frame = vec![0; FRAME_HEADER_BYTES + 28 + FRAME_FOOTER_BYTES];
            Operation::change(ObjectType::Email, ChangeAction::Updated, &[0x11; 16])
                .encode(&mut frame[FRAME_HEADER_BYTES..FRAME_HEADER_BYTES + 28])
                .unwrap();
            frame::seal(&Provider, Sequence::from_u64(segment), 1, &mut frame).unwrap();
            journal.extend_from_slice(&frame);
            total += journal.len() as u64;
            history.push(manifest::HistoryDescriptor {
                segment,
                base: Sequence::from_u64(index as u64),
                through: Sequence::from_u64(segment),
                file_bytes: journal.len() as u64,
                digest: hash(&journal),
            });
            fs::write(path(f, segment), journal).unwrap();
            fs::set_permissions(path(f, segment), fs::Permissions::from_mode(0o600)).unwrap();
        }
        let tables = std::array::from_fn::<_, TABLE_COUNT, _>(|i| {
            manifest
                .table(Table::from_tag(i as u16 + 1).unwrap())
                .unwrap()
        });
        let header = manifest::Header {
            through: Sequence::from_u64((count as u64).max(1)),
            active_segment: count as u64 + 2,
            ..manifest.header()
        };
        let mut encoded = vec![0; format::MAX_MANIFEST_BYTES];
        let n = manifest::encode(&Provider, header, &tables, &history, &mut encoded).unwrap();
        encoded.truncate(n);
        bytes.manifest = encoded;
        Current {
            manifest_digest: hash(&bytes.manifest),
            ..current
        }
        .encode(&Provider, &mut bytes.current)
        .unwrap();
        total
    }
    #[test]
    fn single_multiple_maximum_and_empty_history_reuse_full_slots() {
        let f = Fixture::new();
        let (root, mut bytes) = setup(&f);
        probe(&root, &bytes, &mut [0; MAX_RECORD_BYTES]);
        for count in [0, 2, format::MAX_HISTORY_DESCRIPTORS] {
            let total = segments(&f, &mut bytes, count);
            let mut cells = [Cell::EMPTY; 4];
            let original = cells.as_ptr();
            let mut sweep = HistorySweep::new(
                &root,
                &Provider,
                bytes.selection(),
                &mut cells,
                HistorySweepLimits {
                    bytes: total,
                    frames: count as u64,
                },
            )
            .unwrap();
            let mut opened = 0;
            let mut frames = 0;
            let mut verified = 0;
            for _ in 0..count * 4 + 1 {
                match sweep.advance(&mut [0; MAX_RECORD_BYTES]).unwrap() {
                    HistorySweepStep::Opened { segment } => {
                        opened += 1;
                        assert_eq!(segment, opened);
                    }
                    HistorySweepStep::Frame { segment, sequence } => {
                        frames += 1;
                        assert_eq!(segment, frames);
                        assert_eq!(sequence.number(), frames);
                    }
                    HistorySweepStep::Verified { segment } => {
                        verified += 1;
                        assert_eq!(segment, verified);
                    }
                    HistorySweepStep::Exhausted { .. } => {}
                    HistorySweepStep::Complete => break,
                }
            }
            assert!(sweep.is_complete());
            assert_eq!(
                (opened, frames, verified),
                (count as u64, count as u64, count as u64)
            );
            let (done, cells) = sweep.finish().unwrap();
            assert_eq!((cells.as_ptr(), cells.len()), (original, 4));
            assert_eq!(done.current(), bytes.selection().current());
            assert_eq!(
                (done.segments(), done.frames(), done.bytes()),
                (count, count as u64, total)
            );
        }
        assert!(std::mem::size_of::<HistorySweep<'_, '_, '_, '_, Provider>>() <= 2048);
    }
    #[test]
    fn selected_byte_frame_and_change_capacity_limits_refuse() {
        let f = Fixture::new();
        let (root, mut bytes) = setup(&f);
        let total = segments(&f, &mut bytes, 2);
        let mut cells = [Cell::EMPTY; 1];
        assert!(matches!(
            HistorySweep::new(
                &root,
                &Provider,
                bytes.selection(),
                &mut cells,
                HistorySweepLimits {
                    bytes: total - 1,
                    frames: 2
                }
            ),
            Err(HistorySweepError::ByteLimit)
        ));
        assert!(matches!(
            HistorySweep::new(
                &root,
                &Provider,
                bytes.selection(),
                &mut cells,
                HistorySweepLimits {
                    bytes: total,
                    frames: 1
                }
            ),
            Err(HistorySweepError::FrameLimit)
        ));
        let sweep = HistorySweep::new(
            &root,
            &Provider,
            bytes.selection(),
            &mut cells,
            HistorySweepLimits {
                bytes: total,
                frames: 2,
            },
        )
        .unwrap();
        assert!(matches!(sweep.finish(), Err(HistorySweepError::Incomplete)));
        let mut sweep = HistorySweep::new(
            &root,
            &Provider,
            bytes.selection(),
            &mut [],
            HistorySweepLimits {
                bytes: total,
                frames: 2,
            },
        )
        .unwrap();
        assert!(matches!(
            sweep.advance(&mut [0; MAX_RECORD_BYTES]).unwrap(),
            HistorySweepStep::Opened { .. }
        ));
        assert!(matches!(
            sweep.advance(&mut [0; MAX_RECORD_BYTES]),
            Err(HistorySweepError::ChangeCapacity)
        ));
        assert!(sweep.is_failed());
        assert!(matches!(
            sweep.advance(&mut [0; MAX_RECORD_BYTES]),
            Err(HistorySweepError::Failed)
        ));
        assert!(matches!(sweep.finish(), Err(HistorySweepError::Failed)));
    }
    #[test]
    fn frame_beyond_selected_endpoint_is_not_read() {
        let f = Fixture::new();
        let (root, mut bytes) = setup(&f);
        segments(&f, &mut bytes, 1);
        let mut journal = fs::read(path(&f, 1)).unwrap();
        let mut extra = journal[JOURNAL_HEADER_BYTES..].to_vec();
        frame::seal(&Provider, Sequence::from_u64(2), 1, &mut extra).unwrap();
        journal.extend_from_slice(&extra);
        let selection = bytes.selection();
        let metadata = selection.manifest();
        let current = selection.current();
        let tables = std::array::from_fn::<_, TABLE_COUNT, _>(|i| {
            metadata
                .table(Table::from_tag(i as u16 + 1).unwrap())
                .unwrap()
        });
        let descriptor = manifest::HistoryDescriptor {
            file_bytes: journal.len() as u64,
            digest: hash(&journal),
            ..metadata.history(0).unwrap()
        };
        let mut encoded = vec![0; format::MAX_MANIFEST_BYTES];
        let n = manifest::encode(
            &Provider,
            metadata.header(),
            &tables,
            &[descriptor],
            &mut encoded,
        )
        .unwrap();
        encoded.truncate(n);
        bytes.manifest = encoded;
        Current {
            manifest_digest: hash(&bytes.manifest),
            ..current
        }
        .encode(&Provider, &mut bytes.current)
        .unwrap();
        fs::write(path(&f, 1), &journal).unwrap();
        let mut cells = [Cell::EMPTY; 1];
        let mut sweep = HistorySweep::new(
            &root,
            &Provider,
            bytes.selection(),
            &mut cells,
            HistorySweepLimits {
                bytes: journal.len() as u64,
                frames: 1,
            },
        )
        .unwrap();
        assert!(matches!(
            sweep.advance(&mut [0; MAX_RECORD_BYTES]).unwrap(),
            HistorySweepStep::Opened { .. }
        ));
        assert!(
            matches!(sweep.advance(&mut [0; MAX_RECORD_BYTES]).unwrap(), HistorySweepStep::Frame { sequence, .. } if sequence.number() == 1)
        );
        let position = sweep.input.as_ref().unwrap().position();
        assert!(matches!(
            sweep.advance(&mut [0; MAX_RECORD_BYTES]).unwrap(),
            HistorySweepStep::Exhausted { segment: 1 }
        ));
        assert_eq!(sweep.input.as_ref().unwrap().position(), position);
        assert!(matches!(
            sweep.advance(&mut [0; MAX_RECORD_BYTES]),
            Err(HistorySweepError::Input(HistoryInputError::Io(e)))
                if e.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(matches!(sweep.finish(), Err(HistorySweepError::Failed)));
    }
    #[test]
    fn missing_late_segment_and_selected_digest_failure_never_complete() {
        for digest_error in [false, true] {
            let f = Fixture::new();
            let (root, mut bytes) = setup(&f);
            let total = segments(&f, &mut bytes, 2);
            if digest_error {
                let selection = bytes.selection();
                let manifest = selection.manifest();
                let current = selection.current();
                let tables = std::array::from_fn::<_, TABLE_COUNT, _>(|i| {
                    manifest
                        .table(Table::from_tag(i as u16 + 1).unwrap())
                        .unwrap()
                });
                let mut history = [manifest.history(0).unwrap(), manifest.history(1).unwrap()];
                history[1].digest[0] ^= 1;
                let mut encoded = vec![0; format::MAX_MANIFEST_BYTES];
                let n = manifest::encode(
                    &Provider,
                    manifest.header(),
                    &tables,
                    &history,
                    &mut encoded,
                )
                .unwrap();
                encoded.truncate(n);
                bytes.manifest = encoded;
                Current {
                    manifest_digest: hash(&bytes.manifest),
                    ..current
                }
                .encode(&Provider, &mut bytes.current)
                .unwrap();
            } else {
                fs::remove_file(path(&f, 2)).unwrap();
            }
            let mut cells = [Cell::EMPTY; 1];
            let mut sweep = HistorySweep::new(
                &root,
                &Provider,
                bytes.selection(),
                &mut cells,
                HistorySweepLimits {
                    bytes: total,
                    frames: 2,
                },
            )
            .unwrap();
            let mut verified = 0;
            let mut failed = false;
            for _ in 0..10 {
                match sweep.advance(&mut [0; MAX_RECORD_BYTES]) {
                    Ok(HistorySweepStep::Verified { .. }) => verified += 1,
                    Err(HistorySweepError::Input(_)) => {
                        failed = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => panic!("unexpected: {e}"),
                }
            }
            assert!(failed);
            assert_eq!(verified, 1);
            assert!(matches!(sweep.finish(), Err(HistorySweepError::Failed)));
        }
    }
}
