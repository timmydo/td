//! Complete selected physical inputs before exposing an offline validation snapshot.
use super::super::{
    ChangeRoute, CompleteHistorySweep, CompleteTables, HistorySweep, HistorySweepError,
    HistorySweepLimits, HistorySweepStep, LoadedOverlay, TableSweep, TableSweepError,
    TableSweepLimits, TableSweepStep,
};
use super::StoppedStore;
use crate::{
    format::{bindings::Selection, container::Current, table::MAX_RECORD_BYTES},
    frame_changes::Cell,
    ports::{Crypto, Error as PolicyError, ViewIdentity},
};

#[path = "view.rs"]
mod view;
pub use view::{ReadLimits, ValidationReadRequest, ValidationView};

#[derive(Debug)]
pub enum ValidationError {
    Owner,
    Policy(PolicyError),
    Tables(TableSweepError),
    History(HistorySweepError),
    Failed,
    Incomplete,
}
impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stopped selected-file validation: {self:?}")
    }
}
impl std::error::Error for ValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Policy(e) => Some(e),
            Self::Tables(e) => Some(e),
            Self::History(e) => Some(e),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidationLimits {
    pub tables: TableSweepLimits,
    pub history: HistorySweepLimits,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationStep {
    Table(TableSweepStep),
    History(HistorySweepStep),
    Complete,
}

pub struct FileValidation<'r, 'c, 'm, 't, 'h, 'o, 'b, 's, C: Crypto> {
    store: &'r StoppedStore,
    selection: Selection<'m>,
    active: &'o LoadedOverlay<'r, 'b, 's>,
    tables: Option<TableSweep<'r, 'c, 'm, 't, 'o, 'b, 's, C>>,
    history: Option<HistorySweep<'r, 'c, 'm, 'h, C>>,
    scratch: Option<&'t mut [u8; MAX_RECORD_BYTES]>,
    cells: Option<&'h mut [Cell]>,
    complete_tables: Option<CompleteTables>,
    complete_history: Option<CompleteHistorySweep>,
    complete: bool,
    failed: bool,
}
impl StoppedStore {
    /// Admit both sweeps before any table/history I/O. Active loading is a prior step.
    pub fn validate_files<'r, 'c, 'm, 't, 'h, 'o, 'b, 's, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'m>,
        active: &'o LoadedOverlay<'r, 'b, 's>,
        scratch: &'t mut [u8; MAX_RECORD_BYTES],
        cells: &'h mut [Cell],
        limits: ValidationLimits,
    ) -> Result<FileValidation<'r, 'c, 'm, 't, 'h, 'o, 'b, 's, C>, ValidationError> {
        if !active.file().belongs_to(&self.root) {
            return Err(ValidationError::Owner);
        }
        ChangeRoute::new(selection, active.identity()).map_err(ValidationError::Policy)?;
        let history = self
            .history(crypto, selection, cells, limits.history)
            .map_err(ValidationError::History)?;
        let tables = self
            .tables(crypto, selection, active, scratch, limits.tables)
            .map_err(ValidationError::Tables)?;
        Ok(FileValidation {
            store: self,
            selection,
            active,
            tables: Some(tables),
            history: Some(history),
            scratch: None,
            cells: None,
            complete_tables: None,
            complete_history: None,
            complete: false,
            failed: false,
        })
    }
}
impl<'r, 'm, 't, 'h, 'o, 'b, 's, C: Crypto> FileValidation<'r, '_, 'm, 't, 'h, 'o, 'b, 's, C> {
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    pub const fn is_complete(&self) -> bool {
        self.complete && !self.failed
    }
    /// One underlying sweep step; transferred scratch is reused by history.
    pub fn advance(&mut self) -> Result<ValidationStep, ValidationError> {
        if self.failed {
            return Err(ValidationError::Failed);
        }
        self.failed = true;
        let step = self.work()?;
        self.failed = false;
        Ok(step)
    }
    fn work(&mut self) -> Result<ValidationStep, ValidationError> {
        if self.complete {
            return Ok(ValidationStep::Complete);
        }
        if let Some(tables) = self.tables.as_mut() {
            let step = tables.advance().map_err(ValidationError::Tables)?;
            if step == TableSweepStep::Complete {
                let (complete, scratch) = self
                    .tables
                    .take()
                    .ok_or(ValidationError::Incomplete)?
                    .finish()
                    .map_err(ValidationError::Tables)?;
                self.complete_tables = Some(complete);
                self.scratch = Some(scratch);
            }
            return Ok(ValidationStep::Table(step));
        }
        if let Some(history) = self.history.as_mut() {
            let scratch = self.scratch.as_mut().ok_or(ValidationError::Incomplete)?;
            let step = history.advance(scratch).map_err(ValidationError::History)?;
            if step == HistorySweepStep::Complete {
                let (complete, cells) = self
                    .history
                    .take()
                    .ok_or(ValidationError::Incomplete)?
                    .finish()
                    .map_err(ValidationError::History)?;
                self.complete_history = Some(complete);
                self.cells = Some(cells);
            }
            return Ok(ValidationStep::History(step));
        }
        self.complete = true;
        Ok(ValidationStep::Complete)
    }
    pub fn finish(
        self,
    ) -> Result<
        (
            CheckedFiles<'r, 'm, 'o, 'b, 's>,
            &'t mut [u8; MAX_RECORD_BYTES],
            &'h mut [Cell],
        ),
        ValidationError,
    > {
        if self.failed {
            return Err(ValidationError::Failed);
        }
        if !self.complete {
            return Err(ValidationError::Incomplete);
        }
        Ok((
            CheckedFiles {
                store: self.store,
                selection: self.selection,
                active: self.active,
                tables: self.complete_tables.ok_or(ValidationError::Incomplete)?,
                history: self.complete_history.ok_or(ValidationError::Incomplete)?,
            },
            self.scratch.ok_or(ValidationError::Incomplete)?,
            self.cells.ok_or(ValidationError::Incomplete)?,
        ))
    }
}
/// Holds the stopped owner and active overlay. Final cross-row/blob checks remain.
pub struct CheckedFiles<'r, 'm, 'o, 'b, 's> {
    store: &'r StoppedStore,
    selection: Selection<'m>,
    active: &'o LoadedOverlay<'r, 'b, 's>,
    tables: CompleteTables,
    history: CompleteHistorySweep,
}
impl CheckedFiles<'_, '_, '_, '_, '_> {
    pub fn identity(&self) -> ViewIdentity {
        self.active.identity()
    }
    pub fn current(&self) -> Current {
        self.selection.current()
    }
    pub const fn tables(&self) -> &CompleteTables {
        &self.tables
    }
    pub const fn history(&self) -> &CompleteHistorySweep {
        &self.history
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::super::{active, tests::Fixture};
    use super::*;
    use crate::{
        format::{Sequence, Table},
        ids::AccountId,
        overlay,
        store_paths::{AccountEntry, Name, Number},
    };
    use td_crypto::Provider;
    pub(super) fn prepare() -> (Fixture, StoppedStore, active::ProbeBytes) {
        super::super::tests::prepare()
    }

    pub(super) fn limits() -> ValidationLimits {
        ValidationLimits {
            tables: TableSweepLimits {
                bytes: 1345,
                rows: 0,
            },
            history: HistorySweepLimits {
                bytes: 277,
                frames: 1,
            },
        }
    }
    #[test]
    fn all_physical_inputs_complete_before_reclaiming_both_buffers() {
        let (_fixture, store, bytes) = prepare();
        let mut selected_scratch = super::super::super::SelectionScratch::new();
        let selected = store
            .load_selection(
                &Provider,
                AccountId::from_bytes([0x33; 16]),
                &mut selected_scratch,
            )
            .unwrap();
        let mut frames = [0; 160];
        let mut overlay = [overlay::Cell::EMPTY; 2];
        let active = store
            .load_active_overlay(
                &Provider,
                selected,
                bytes.view(2, 256),
                256,
                &mut frames,
                &mut overlay,
            )
            .unwrap();
        let mut scratch = [0; MAX_RECORD_BYTES];
        let record_address = scratch.as_ptr();
        let mut cells = [Cell::EMPTY; 2];
        let cells_address = cells.as_ptr();
        let mut validation = store
            .validate_files(
                &Provider,
                selected,
                &active,
                &mut scratch,
                &mut cells,
                limits(),
            )
            .unwrap();
        let mut tables_done = false;
        let mut history_done = false;
        let mut history_started = false;
        let mut table_steps = (0, 0, 0, 0, 0);
        let mut history_steps = (0, 0, 0, 0, 0);
        for _ in 0..64 {
            match validation.advance().unwrap() {
                ValidationStep::Table(step) => {
                    assert!(!history_started && !tables_done);
                    match step {
                        TableSweepStep::Opened(_) => table_steps.0 += 1,
                        TableSweepStep::Record(_) => table_steps.1 += 1,
                        TableSweepStep::Exhausted(_) => table_steps.2 += 1,
                        TableSweepStep::Verified { .. } => table_steps.3 += 1,
                        TableSweepStep::Complete => {
                            table_steps.4 += 1;
                            tables_done = true;
                        }
                    }
                }
                ValidationStep::History(step) => {
                    assert!(tables_done && !history_done);
                    history_started = true;
                    match step {
                        HistorySweepStep::Opened { .. } => history_steps.0 += 1,
                        HistorySweepStep::Frame { .. } => history_steps.1 += 1,
                        HistorySweepStep::Exhausted { .. } => history_steps.2 += 1,
                        HistorySweepStep::Verified { .. } => history_steps.3 += 1,
                        HistorySweepStep::Complete => {
                            history_steps.4 += 1;
                            history_done = true;
                        }
                    }
                }
                ValidationStep::Complete => {
                    assert!(history_done);
                    break;
                }
            }
        }
        assert_eq!(table_steps, (11, 1, 11, 11, 1));
        assert_eq!(history_steps, (1, 1, 1, 1, 1));
        assert!(validation.is_complete());
        assert_eq!(validation.advance().unwrap(), ValidationStep::Complete);
        let (checked, scratch, cells) = validation.finish().unwrap();
        assert_eq!(scratch.as_ptr(), record_address);
        assert_eq!((cells.as_ptr(), cells.len()), (cells_address, 2));
        assert_eq!(checked.current(), selected.current());
        assert_eq!(checked.identity(), bytes.view(2, 256));
        assert_eq!(checked.tables().rows(), 0);
        assert_eq!(checked.tables().table_bytes(), 1345);
        assert_eq!(checked.history().frames(), 1);
        assert_eq!(checked.history().segments(), 1);
        assert_eq!(checked.history().bytes(), 277);
        assert_eq!(checked.history().checkpoint(), Sequence::from_u64(1));
        assert!(
            std::mem::size_of::<FileValidation<'_, '_, '_, '_, '_, '_, '_, '_, Provider>>() <= 8192
        );
    }
    #[test]
    fn captured_history_floor_policy_precedes_file_io() {
        let (fixture, store, bytes) = prepare();
        let mut frames = [0; 160];
        let mut overlay = [overlay::Cell::EMPTY; 2];
        let mut identity = bytes.view(2, 256);
        identity.history_floor = Sequence::from_u64(3);
        let active = store
            .load_active_overlay(
                &Provider,
                bytes.selection(),
                identity,
                256,
                &mut frames,
                &mut overlay,
            )
            .unwrap();
        let name = Name::account(
            AccountId::from_bytes([0x33; 16]),
            AccountEntry::Table(Number::new(2).unwrap(), Table::Blobs),
        )
        .unwrap();
        std::fs::remove_file(fixture.path.join(name.as_path().unwrap())).unwrap();
        assert!(matches!(
            store.validate_files(
                &Provider,
                bytes.selection(),
                &active,
                &mut [0; MAX_RECORD_BYTES],
                &mut [Cell::EMPTY; 2],
                limits()
            ),
            Err(ValidationError::Policy(PolicyError::Invalid))
        ));
    }
    #[test]
    fn another_owner_and_history_admission_refuse_before_table_io() {
        let (fixture, store, bytes) = prepare();
        let other_fixture = Fixture::new();
        let other = StoppedStore::new(other_fixture.locked());
        let mut frames = [0; 160];
        let mut overlay = [overlay::Cell::EMPTY; 2];
        let active = store
            .load_active_overlay(
                &Provider,
                bytes.selection(),
                bytes.view(2, 256),
                256,
                &mut frames,
                &mut overlay,
            )
            .unwrap();
        let mut scratch = [0; MAX_RECORD_BYTES];
        let mut cells = [Cell::EMPTY; 2];
        assert!(matches!(
            other.validate_files(
                &Provider,
                bytes.selection(),
                &active,
                &mut scratch,
                &mut cells,
                limits()
            ),
            Err(ValidationError::Owner)
        ));
        let name = Name::account(
            AccountId::from_bytes([0x33; 16]),
            AccountEntry::Table(Number::new(2).unwrap(), Table::Blobs),
        )
        .unwrap();
        std::fs::remove_file(fixture.path.join(name.as_path().unwrap())).unwrap();
        let mut budget = limits();
        budget.history.bytes = 276;
        assert!(matches!(
            store.validate_files(
                &Provider,
                bytes.selection(),
                &active,
                &mut scratch,
                &mut cells,
                budget
            ),
            Err(ValidationError::History(HistorySweepError::ByteLimit))
        ));
        let mut validation = store
            .validate_files(
                &Provider,
                bytes.selection(),
                &active,
                &mut scratch,
                &mut cells,
                limits(),
            )
            .unwrap();
        assert!(matches!(
            validation.advance(),
            Err(ValidationError::Tables(_))
        ));
        assert!(validation.is_failed());
        assert!(matches!(validation.advance(), Err(ValidationError::Failed)));
        assert!(matches!(validation.finish(), Err(ValidationError::Failed)));
    }
    #[test]
    fn late_history_failure_and_premature_finish_produce_no_snapshot() {
        let (fixture, store, bytes) = prepare();
        let mut frames = [0; 160];
        let mut overlay = [overlay::Cell::EMPTY; 2];
        let active = store
            .load_active_overlay(
                &Provider,
                bytes.selection(),
                bytes.view(2, 256),
                256,
                &mut frames,
                &mut overlay,
            )
            .unwrap();
        let mut scratch = [0; MAX_RECORD_BYTES];
        let mut cells = [Cell::EMPTY; 2];
        let validation = store
            .validate_files(
                &Provider,
                bytes.selection(),
                &active,
                &mut scratch,
                &mut cells,
                limits(),
            )
            .unwrap();
        assert!(matches!(
            validation.finish(),
            Err(ValidationError::Incomplete)
        ));
        let name = Name::account(
            AccountId::from_bytes([0x33; 16]),
            AccountEntry::Journal(Number::new(1).unwrap()),
        )
        .unwrap();
        std::fs::remove_file(fixture.path.join(name.as_path().unwrap())).unwrap();
        let mut validation = store
            .validate_files(
                &Provider,
                bytes.selection(),
                &active,
                &mut scratch,
                &mut cells,
                limits(),
            )
            .unwrap();
        let mut tables_done = false;
        let mut failed = false;
        for _ in 0..64 {
            match validation.advance() {
                Ok(ValidationStep::Table(TableSweepStep::Complete)) => tables_done = true,
                Err(ValidationError::History(_)) => {
                    failed = true;
                    break;
                }
                other => {
                    other.unwrap();
                }
            }
        }
        assert!(tables_done && failed);
        assert!(matches!(validation.finish(), Err(ValidationError::Failed)));
    }
}
