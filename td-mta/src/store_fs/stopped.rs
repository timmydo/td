//! Read-only ownership of a cooperatively locked store during offline validation.
use super::{
    BlobSweep, ChangeInputError, ChangeScan, ChangeScanRequest, HistorySweep, HistorySweepError,
    HistorySweepLimits, LoadedOverlay, LockedRoot, OverlayInputError, SelectionError,
    SelectionScratch, TableInput, TableInputError, TableSweep, TableSweepError, TableSweepLimits,
};
use crate::{
    format::{bindings::Selection, table::MAX_RECORD_BYTES, Table},
    frame_changes,
    ids::AccountId,
    overlay,
    ports::{Crypto, ViewIdentity},
};

#[path = "stopped/capture.rs"]
mod capture;
pub use capture::{CaptureError, CaptureStep, CapturedJournal, JournalCapture};

#[path = "stopped/validation.rs"]
mod validation;
pub use validation::{
    CheckedData, CheckedFiles, DataError, DataLimits, DataStep, DataValidation, FileValidation,
    ReadLimits, ValidationError, ValidationLimits, ValidationReadRequest, ValidationStep,
    ValidationView,
};

#[path = "stopped/verify.rs"]
mod verify;
#[cfg(test)]
pub use verify::{probe_journal_publication, probe_pinned_reads, probe_verify_account};
pub use verify::{
    CommitError, CommittedView, JournalError, JournalSession, JournalStart, JournalStartScratch,
    OwnedVerifyError, PinnedReadError, PinnedReadRequest, PinnedReadScratch, VerifiedAccount,
    VerifiedStore, VerifyError, VerifyLimits, VerifyScratch,
};

/// Consumes the mutation-capable owner; no root/file-handle accessor is exposed.
/// Stable operator-controlled paths and cooperating external writers are still required.
pub struct StoppedStore {
    root: LockedRoot,
}
impl StoppedStore {
    /// Existing borrows, including writable temporaries, must end before transfer.
    ///
    /// ```compile_fail
    /// use td_mta::{ids::AccountId, store_fs::{LockedRoot, StoppedStore}, store_paths::Number};
    /// fn transfer(root: LockedRoot, account: AccountId, number: Number) {
    ///     let mut output = root.create_temporary(account, number, 10).unwrap();
    ///     let stopped = StoppedStore::new(root);
    ///     output.write(b"late").unwrap();
    ///     drop(stopped);
    /// }
    /// ```
    pub fn new(root: LockedRoot) -> Self {
        Self { root }
    }
    /// Consuming thaw requires all borrowed inputs and selected metadata to end.
    ///
    /// ```compile_fail
    /// use td_mta::{ids::AccountId, store_fs::{StoppedStore, SelectionScratch}};
    /// fn thaw(store: StoppedStore, account: AccountId, scratch: &mut SelectionScratch) {
    ///     let selection = store.load_selection(&td_crypto::Provider, account, scratch).unwrap();
    ///     let root = store.into_locked();
    ///     let _ = selection.current();
    ///     drop(root);
    /// }
    /// ```
    pub fn into_locked(self) -> LockedRoot {
        self.root
    }
    /// Admit bounded metadata work; errors may overwrite scratch. The returned
    /// selection borrows both this read-only owner and caller scratch.
    pub fn load_selection<'a>(
        &'a self,
        crypto: &impl Crypto,
        account: AccountId,
        scratch: &'a mut SelectionScratch,
    ) -> Result<Selection<'a>, SelectionError> {
        self.root.load_selection(crypto, account, scratch)
    }
    /// Admit the full read/hash/sort work unit; failure may overwrite frames/cells.
    pub fn load_active_overlay<'r, 'b, 's>(
        &'r self,
        crypto: &impl Crypto,
        selection: Selection<'_>,
        view: ViewIdentity,
        max_bytes: u64,
        frames: &'b mut [u8],
        cells: &'s mut [overlay::Cell],
    ) -> Result<LoadedOverlay<'r, 'b, 's>, OverlayInputError> {
        self.root
            .load_active_overlay(crypto, selection, view, max_bytes, frames, cells)
    }
    fn blobs<'r, 'c, C: Crypto>(
        &'r self,
        crypto: &'c C,
        identity: ViewIdentity,
        rows: u64,
        bytes: u64,
    ) -> BlobSweep<'r, 'c, C> {
        BlobSweep::new(&self.root, crypto, identity, rows, bytes)
    }
    // Keep the validation child on the read-only owner's surface.
    fn open_table<'r, 'c, 'm, 'b, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'m>,
        table: Table,
        max_bytes: u64,
        scratch: &'b mut [u8; MAX_RECORD_BYTES],
    ) -> Result<TableInput<'r, 'c, 'm, 'b, C>, TableInputError> {
        self.root
            .open_table(crypto, selection, table, max_bytes, scratch)
    }
    fn change_scan<'r, 'c, 'm, 'b, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'m>,
        request: ChangeScanRequest,
        cells: &'b mut [frame_changes::Cell],
    ) -> Result<ChangeScan<'r, 'c, 'm, 'b, C>, ChangeInputError> {
        self.root.change_scan(crypto, selection, request, cells)
    }
    /// Admit each replay step and retain any independently supplied overlay owner.
    pub fn tables<'r, 'c, 'm, 't, 'o, 'b, 's, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'m>,
        active: &'o LoadedOverlay<'r, 'b, 's>,
        scratch: &'t mut [u8; MAX_RECORD_BYTES],
        limits: TableSweepLimits,
    ) -> Result<TableSweep<'r, 'c, 'm, 't, 'o, 'b, 's, C>, TableSweepError> {
        TableSweep::new(&self.root, crypto, selection, active, scratch, limits)
    }
    /// Admit each bounded frame step; errors may overwrite record/change scratch.
    pub fn history<'r, 'c, 'm, 'b, C: Crypto>(
        &'r self,
        crypto: &'c C,
        selection: Selection<'m>,
        cells: &'b mut [frame_changes::Cell],
        limits: HistorySweepLimits,
    ) -> Result<HistorySweep<'r, 'c, 'm, 'b, C>, HistorySweepError> {
        HistorySweep::new(&self.root, crypto, selection, cells, limits)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{acquire_lock, active, selection, tests::Fixture, LockError};
    use super::*;
    use crate::{
        format::Sequence,
        store_paths::{AccountEntry, Name, Number},
    };
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use td_crypto::Provider;

    #[test]
    fn transfer_retains_lock_and_consuming_return_restores_mutation() {
        let fixture = Fixture::new();
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        selection::prepare_probe(&root);
        let directory = super::super::Directory::from_path(fixture.path.to_str().unwrap()).unwrap();
        let owner = directory.metadata().unwrap().uid();
        let store = StoppedStore::new(root);
        assert!(matches!(
            acquire_lock(&directory, owner),
            Err(LockError::Busy)
        ));
        let account = AccountId::from_bytes([0x33; 16]);
        let mut scratch = SelectionScratch::new();
        let selected = store
            .load_selection(&Provider, account, &mut scratch)
            .unwrap();
        assert_eq!(selected.current().account, account);
        assert_eq!(selected.current().generation, 1);
        let root = store.into_locked();
        assert!(matches!(
            acquire_lock(&directory, owner),
            Err(LockError::Busy)
        ));
        root.create_account_directory(account, AccountEntry::Temporary)
            .unwrap();
        drop(root);
        drop(fixture.reacquire());
    }

    pub(super) fn prepare() -> (Fixture, StoppedStore, active::ProbeBytes) {
        prepare_fixture(Fixture::new())
    }
    pub(super) fn prepare_fixture(fixture: Fixture) -> (Fixture, StoppedStore, active::ProbeBytes) {
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        selection::prepare_probe(&root);
        let account = AccountId::from_bytes([0x33; 16]);
        drop(super::super::table::prepare_probe(&root));
        drop(super::super::history::prepare_probe(&root));
        let active = active::prepare_probe(&root);
        for (entry, text) in [
            (
                AccountEntry::Current,
                include_str!("../../tests/fixtures/format-v1/current-history.hex"),
            ),
            (
                AccountEntry::Manifest(Number::new(2).unwrap()),
                include_str!("../../tests/fixtures/format-v1/manifest-history.hex"),
            ),
        ] {
            let path = fixture
                .path
                .join(Name::account(account, entry).unwrap().as_path().unwrap());
            std::fs::write(&path, active::fixture::hex(text)).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        (fixture, StoppedStore::new(root), active)
    }

    #[test]
    fn owner_loaded_selection_flows_through_history_overlay_and_tables() {
        let (_fixture, store, active) = prepare();
        let account = AccountId::from_bytes([0x33; 16]);
        let mut selection_scratch = SelectionScratch::new();
        let selected = store
            .load_selection(&Provider, account, &mut selection_scratch)
            .unwrap();
        assert_eq!(selected.current(), active.selection().current());
        let mut changes = [frame_changes::Cell::EMPTY; 2];
        let mut scratch = [0; MAX_RECORD_BYTES];
        let mut scan = store
            .history(
                &Provider,
                selected,
                &mut changes,
                HistorySweepLimits {
                    bytes: 277,
                    frames: 1,
                },
            )
            .unwrap();
        for _ in 0..5 {
            scan.advance(&mut scratch).unwrap();
            if scan.is_complete() {
                break;
            }
        }
        let (complete, _) = scan.finish().unwrap();
        assert_eq!(complete.checkpoint(), Sequence::from_u64(1));
        let mut frames = [0; 160];
        let mut cells = [overlay::Cell::EMPTY; 2];
        let loaded = store
            .load_active_overlay(
                &Provider,
                selected,
                active.view(2, 256),
                256,
                &mut frames,
                &mut cells,
            )
            .unwrap();
        assert_eq!(loaded.identity().account, account);
        assert_eq!(loaded.identity().committed_sequence, Sequence::from_u64(2));
        let mut scan = store
            .tables(
                &Provider,
                selected,
                &loaded,
                &mut scratch,
                TableSweepLimits {
                    bytes: 1345,
                    rows: 0,
                },
            )
            .unwrap();
        for _ in 0..46 {
            scan.advance().unwrap();
            if scan.is_complete() {
                break;
            }
        }
        let (complete, _) = scan.finish().unwrap();
        assert_eq!(complete.rows(), 0);
        drop(loaded);
        drop(store.into_locked());
    }
}
