//! Capture a stopped journal prefix without exposing its mutation-capable repair value.
use super::super::{
    ChangeRoute, LoadedOverlay, OverlayInputError, RecoveryInput, RecoveryInputError,
    ScannedJournal,
};
use super::StoppedStore;
use crate::{
    format::{
        bindings::Selection, container::Current, journal_stream::Summary, Sequence,
        JOURNAL_HEADER_BYTES, MAX_FRAME_BYTES,
    },
    overlay,
    ports::{Crypto, Error as PolicyError, ViewIdentity},
};

#[derive(Debug)]
pub enum CaptureError {
    Input(RecoveryInputError),
    Policy(PolicyError),
    Failed,
    Incomplete,
}
impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stopped journal capture: {self:?}")
    }
}
impl std::error::Error for CaptureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Input(e) => Some(e),
            Self::Policy(e) => Some(e),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureStep {
    Frame {
        through: Sequence,
        operations: usize,
    },
    End,
}
pub struct JournalCapture<'r, 'c, 'm, 'b, C: Crypto> {
    store: &'r StoppedStore,
    selection: Selection<'m>,
    input: RecoveryInput<'r, 'c, 'b, C>,
    identity: ViewIdentity,
    end: bool,
    failed: bool,
}
impl StoppedStore {
    /// Caller admits each whole-frame step and completion, including deadline checks.
    ///
    /// ```compile_fail
    /// use td_mta::{format::{bindings::Selection, MAX_FRAME_BYTES}, store_fs::StoppedStore};
    /// fn thaw(store: StoppedStore, selection: Selection<'_>, scratch: &mut [u8; MAX_FRAME_BYTES]) {
    ///     let mut scan = store.capture_journal(&td_crypto::Provider, selection, 4096, scratch).unwrap();
    ///     let root = store.into_locked();
    ///     scan.advance().unwrap();
    ///     drop(root);
    /// }
    /// ```
    pub fn capture_journal<'r, 'c, 'm, 'b, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'m>,
        max_bytes: u64,
        scratch: &'b mut [u8; MAX_FRAME_BYTES],
    ) -> Result<JournalCapture<'r, 'c, 'm, 'b, C>, CaptureError> {
        let manifest = selection.manifest();
        let header = manifest.header();
        let history_floor = if manifest.history_count() == 0 {
            header.through
        } else {
            manifest
                .history(0)
                .map_err(|e| CaptureError::Input(e.into()))?
                .base
        };
        let identity = ViewIdentity {
            account: header.account,
            epoch: header.epoch,
            generation: header.generation,
            checkpoint: header.through,
            segment: header.active_segment,
            committed_offset: JOURNAL_HEADER_BYTES as u64,
            committed_sequence: header.through,
            history_floor,
        };
        ChangeRoute::new(selection, identity).map_err(CaptureError::Policy)?;
        let input = self
            .root
            .scan_active_journal(crypto, selection, max_bytes, scratch)
            .map_err(CaptureError::Input)?;
        Ok(JournalCapture {
            store: self,
            selection,
            input,
            identity,
            end: false,
            failed: false,
        })
    }
}
impl<'r, 'm, C: Crypto> JournalCapture<'r, '_, 'm, '_, C> {
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    /// End remains provisional until finish verifies physical EOF.
    pub fn advance(&mut self) -> Result<CaptureStep, CaptureError> {
        if self.failed {
            return Err(CaptureError::Failed);
        }
        if self.end {
            return Ok(CaptureStep::End);
        }
        self.failed = true;
        let frame = self.input.next_frame().map_err(CaptureError::Input)?;
        let step = if let Some(frame) = frame {
            let header = frame.header();
            CaptureStep::Frame {
                through: header.sequence,
                operations: header.operations,
            }
        } else {
            self.end = true;
            CaptureStep::End
        };
        self.failed = false;
        Ok(step)
    }
    pub fn finish(mut self) -> Result<CapturedJournal<'r, 'm>, CaptureError> {
        if self.failed {
            return Err(CaptureError::Failed);
        }
        if !self.end {
            return Err(CaptureError::Incomplete);
        }
        let journal = self.input.finish().map_err(CaptureError::Input)?;
        self.identity.committed_sequence = journal.summary().through();
        self.identity.committed_offset = journal.valid_bytes();
        ChangeRoute::new(self.selection, self.identity).map_err(CaptureError::Policy)?;
        Ok(CapturedJournal {
            store: self.store,
            selection: self.selection,
            journal,
            identity: self.identity,
        })
    }
}
/// A selected complete prefix; an incomplete tail is reported and never repaired here.
///
/// ```compile_fail
/// use td_mta::store_fs::CapturedJournal;
/// fn repair(captured: CapturedJournal<'_, '_>) {
///     captured.repair(&td_crypto::Provider).unwrap();
/// }
/// ```
pub struct CapturedJournal<'r, 'm> {
    store: &'r StoppedStore,
    selection: Selection<'m>,
    journal: ScannedJournal<'r>,
    identity: ViewIdentity,
}
impl<'r, 'm> CapturedJournal<'r, 'm> {
    pub const fn identity(&self) -> ViewIdentity {
        self.identity
    }
    pub fn current(&self) -> Current {
        self.selection.current()
    }
    pub const fn selection(&self) -> Selection<'m> {
        self.selection
    }
    pub fn summary(&self) -> Summary {
        self.journal.summary()
    }
    pub fn physical_bytes(&self) -> u64 {
        self.journal.file().len()
    }
    pub fn has_incomplete_tail(&self) -> bool {
        self.journal.has_incomplete_tail()
    }
    /// Re-read only the verified prefix into the caller's replay arena; admit full load work.
    pub fn load_overlay<'b, 's>(
        &self,
        crypto: &impl Crypto,
        max_bytes: u64,
        frames: &'b mut [u8],
        cells: &'s mut [overlay::Cell],
    ) -> Result<LoadedOverlay<'r, 'b, 's>, OverlayInputError> {
        let loaded = self.store.load_active_overlay(
            crypto,
            self.selection,
            self.identity,
            max_bytes,
            frames,
            cells,
        )?;
        if !crypto.equal_digest(
            &loaded.overlay().summary().digest(),
            &self.journal.summary().digest(),
        ) {
            return Err(crate::format::container::Error::Checksum.into());
        }
        Ok(loaded)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::super::{active::fixture, SelectionScratch};
    use super::super::tests::prepare;
    use super::*;
    use crate::{
        format::{self, manifest, Table, CURRENT_BYTES, MAX_MANIFEST_BYTES, TABLE_COUNT},
        ids::AccountId,
        ports::Digest,
        store_paths::{AccountEntry, Name, Number},
    };
    use td_crypto::Provider;
    fn scratch() -> Box<[u8; MAX_FRAME_BYTES]> {
        vec![0; MAX_FRAME_BYTES]
            .into_boxed_slice()
            .try_into()
            .unwrap()
    }
    fn scan_to_end(scan: &mut JournalCapture<'_, '_, '_, '_, Provider>) -> usize {
        let mut frames = 0;
        for _ in 0..4 {
            match scan.advance().unwrap() {
                CaptureStep::Frame {
                    through,
                    operations,
                } => {
                    frames += 1;
                    assert_eq!(through.number(), 2);
                    assert_eq!(operations, 2);
                }
                CaptureStep::End => return frames,
            }
        }
        frames
    }
    #[test]
    fn captures_exact_empty_complete_and_incomplete_prefixes_without_repair() {
        for mode in 0..4 {
            let (fixture_dir, store, _bytes) = prepare();
            let mut journal = fixture::journal();
            if mode == 0 {
                journal.truncate(96);
            }
            if mode == 2 {
                journal.extend_from_slice(b"partial");
            }
            if mode == 3 {
                let mut next = journal[96..].to_vec();
                format::frame::seal(&Provider, Sequence::from_u64(3), 2, &mut next).unwrap();
                journal.extend_from_slice(&next[..70]);
            }
            fixture::write(&store.root, &journal);
            let mut metadata = SelectionScratch::new();
            let selection = store
                .load_selection(&Provider, AccountId::from_bytes([0x33; 16]), &mut metadata)
                .unwrap();
            let mut scratch = scratch();
            let mut scan = store
                .capture_journal(&Provider, selection, journal.len() as u64, &mut scratch)
                .unwrap();
            assert!(std::mem::size_of_val(&scan) <= 2048);
            assert_eq!(scan_to_end(&mut scan), usize::from(mode != 0));
            assert_eq!(scan.advance().unwrap(), CaptureStep::End);
            let captured = scan.finish().unwrap();
            scratch.fill(0);
            assert!(std::mem::size_of_val(&captured) <= 2048);
            assert_eq!(captured.current(), selection.current());
            assert_eq!(captured.identity().history_floor, Sequence::default());
            assert_eq!(
                captured.identity().committed_sequence.number(),
                if mode == 0 { 1 } else { 2 }
            );
            assert_eq!(
                captured.identity().committed_offset,
                if mode == 0 { 96 } else { 256 }
            );
            assert_eq!(captured.physical_bytes(), journal.len() as u64);
            assert_eq!(captured.has_incomplete_tail(), mode >= 2);
            assert_eq!(
                captured.summary().through(),
                captured.identity().committed_sequence
            );
            let mut cells = [overlay::Cell::EMPTY; 2];
            let active = captured
                .load_overlay(
                    &Provider,
                    captured.identity().committed_offset,
                    &mut scratch[..160],
                    &mut cells,
                )
                .unwrap();
            assert_eq!(active.identity(), captured.identity());
            let name = Name::account(
                captured.identity().account,
                AccountEntry::Journal(Number::new(2).unwrap()),
            )
            .unwrap();
            assert_eq!(
                std::fs::read(fixture_dir.path.join(name.as_path().unwrap())).unwrap(),
                journal
            );
        }
    }
    #[test]
    fn absent_history_uses_checkpoint_as_floor_and_drives_physical_validation() {
        let (fixture_dir, store, bytes) = prepare();
        let selected = bytes.selection();
        let manifest = selected.manifest();
        let tables = std::array::from_fn::<_, TABLE_COUNT, _>(|i| {
            manifest
                .table(Table::from_tag((i + 1) as u16).unwrap())
                .unwrap()
        });
        let mut encoded = [0; MAX_MANIFEST_BYTES];
        let length =
            manifest::encode(&Provider, manifest.header(), &tables, &[], &mut encoded).unwrap();
        let mut current = selected.current();
        let mut digest = Provider.sha256().unwrap();
        digest.update(&encoded[..length]).unwrap();
        current.manifest_digest = digest.finish().unwrap();
        let mut selector = [0; CURRENT_BYTES];
        current.encode(&Provider, &mut selector).unwrap();
        for (entry, data) in [
            (AccountEntry::Current, selector.as_slice()),
            (
                AccountEntry::Manifest(Number::new(2).unwrap()),
                &encoded[..length],
            ),
        ] {
            let name = Name::account(current.account, entry).unwrap();
            std::fs::write(fixture_dir.path.join(name.as_path().unwrap()), data).unwrap();
        }
        fixture::write(&store.root, &fixture::journal());
        let mut metadata = SelectionScratch::new();
        let selection = store
            .load_selection(&Provider, current.account, &mut metadata)
            .unwrap();
        let mut scratch = scratch();
        let mut scan = store
            .capture_journal(&Provider, selection, 256, &mut scratch)
            .unwrap();
        assert_eq!(scan_to_end(&mut scan), 1);
        let captured = scan.finish().unwrap();
        assert_eq!(
            captured.identity().history_floor,
            selection.manifest().header().through
        );
        let mut cells = [overlay::Cell::EMPTY; 2];
        let active = captured
            .load_overlay(&Provider, 256, &mut scratch[..160], &mut cells)
            .unwrap();
        let identity = captured.identity();
        drop(captured);
        let mut record = [0; format::table::MAX_RECORD_BYTES];
        let mut changes = [crate::frame_changes::Cell::EMPTY; 2];
        let mut validation = store
            .validate_files(
                &Provider,
                selection,
                &active,
                &mut record,
                &mut changes,
                super::super::ValidationLimits {
                    tables: super::super::super::TableSweepLimits {
                        bytes: 1345,
                        rows: 0,
                    },
                    history: super::super::super::HistorySweepLimits {
                        bytes: 0,
                        frames: 0,
                    },
                },
            )
            .unwrap();
        for _ in 0..64 {
            validation.advance().unwrap();
            if validation.is_complete() {
                break;
            }
        }
        let (files, _, _) = validation.finish().unwrap();
        assert_eq!(files.identity(), identity);
        assert_eq!(files.tables().rows(), 0);
        assert_eq!(files.history().segments(), 0);
    }
    #[test]
    fn same_length_valid_replacement_refuses_captured_digest() {
        let (_fixture_dir, store, bytes) = prepare();
        let mut journal = fixture::journal();
        fixture::write(&store.root, &journal);
        let mut scratch = scratch();
        let mut scan = store
            .capture_journal(&Provider, bytes.selection(), 256, &mut scratch)
            .unwrap();
        assert_eq!(scan_to_end(&mut scan), 1);
        let captured = scan.finish().unwrap();
        let key_offset =
            JOURNAL_HEADER_BYTES + format::FRAME_HEADER_BYTES + format::OPERATION_HEADER_BYTES;
        journal[key_offset..key_offset + 16].fill(0x45);
        format::frame::seal(&Provider, Sequence::from_u64(2), 2, &mut journal[96..]).unwrap();
        fixture::write(&store.root, &journal);
        let mut cells = [overlay::Cell::EMPTY; 2];
        assert!(matches!(
            captured.load_overlay(&Provider, 256, &mut scratch[..160], &mut cells),
            Err(OverlayInputError::Stream(
                format::journal_stream::Error::Journal(format::container::Error::Checksum)
            ))
        ));
    }
    #[test]
    fn corruption_short_capacity_incomplete_finish_and_late_file_changes_refuse() {
        for mode in 0..5 {
            let (_fixture_dir, store, bytes) = prepare();
            let mut journal = fixture::journal();
            if mode == 0 {
                let last = journal.len() - 1;
                journal[last] ^= 1;
            }
            fixture::write(&store.root, &journal);
            let mut scratch = scratch();
            let result = store.capture_journal(
                &Provider,
                bytes.selection(),
                if mode == 1 { 255 } else { 256 },
                &mut scratch,
            );
            if mode == 1 {
                assert!(matches!(result, Err(CaptureError::Input(_))));
                continue;
            }
            let mut scan = result.unwrap();
            if mode == 0 {
                assert!(matches!(scan.advance(), Err(CaptureError::Input(_))));
                assert!(scan.is_failed());
                assert!(matches!(scan.advance(), Err(CaptureError::Failed)));
                assert!(matches!(scan.finish(), Err(CaptureError::Failed)));
                continue;
            }
            if mode == 2 {
                assert!(matches!(scan.finish(), Err(CaptureError::Incomplete)));
                continue;
            }
            assert_eq!(scan_to_end(&mut scan), 1);
            if mode == 3 {
                journal.push(0);
            } else {
                journal.pop();
            }
            fixture::write(&store.root, &journal);
            assert!(matches!(scan.finish(), Err(CaptureError::Input(_))));
        }
    }
}
