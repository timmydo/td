//! One bounded, read-only verification of CURRENT and its selected account data.
use super::super::{
    CompleteBlobSweep, CompleteHistorySweep, CompleteTables, OverlayInputError, SelectionError,
    SelectionScratch,
};
use super::{
    CaptureError, CaptureStep, DataError, DataLimits, ReadLimits, StoppedStore, ValidationError,
    ValidationLimits, ValidationReadRequest,
};
use crate::{
    format::{
        container::Current, journal_stream::Summary, table::MAX_RECORD_BYTES, ObjectType,
        MAX_FRAME_BYTES,
    },
    frame_changes,
    ids::AccountId,
    mailbox_sweep, overlay,
    ports::{ChangeCursor, Clock, Crypto, Deadline, Error as PolicyError, Time, ViewIdentity},
    recipient_sweep, reference_sweep,
};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "verify/publication.rs"]
mod publication;
#[cfg(test)]
pub use publication::{probe_journal_publication, probe_pinned_reads};
pub use publication::{
    CommitError, CommittedView, JournalError, JournalSession, JournalStart, JournalStartScratch,
    PinnedReadError, PinnedReadRequest, PinnedReadScratch, PooledRead, ReadPoolError,
    ReadScratchPool, ReadScratchSlot,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifyLimits {
    pub capture_bytes: u64,
    pub files: ValidationLimits,
    pub read: ReadLimits,
    pub data: DataLimits,
    pub deadline: Deadline,
}
/// All partitions are allocated by the caller before entering verification.
pub struct VerifyScratch<'a> {
    pub selection: &'a mut SelectionScratch,
    pub frame: &'a mut [u8; MAX_FRAME_BYTES],
    pub overlay_frames: &'a mut [u8],
    pub overlay_cells: &'a mut [overlay::Cell],
    pub record: &'a mut [u8; MAX_RECORD_BYTES],
    pub changes: &'a mut [frame_changes::Cell],
    pub key: &'a mut [u8],
    pub value: &'a mut [u8],
}
#[derive(Debug)]
pub enum VerifyError {
    Policy(PolicyError),
    Selection(SelectionError),
    Capture(CaptureError),
    Overlay(OverlayInputError),
    Files(ValidationError),
    Data(DataError),
}
impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stopped account verification: {self:?}")
    }
}
impl std::error::Error for VerifyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Policy(e) => Some(e),
            Self::Selection(e) => Some(e),
            Self::Capture(e) => Some(e),
            Self::Overlay(e) => Some(e),
            Self::Files(e) => Some(e),
            Self::Data(e) => Some(e),
        }
    }
}
/// Retains stopped ownership, not scratch. Tail repair and activation remain separate.
///
/// ```compile_fail
/// use td_mta::{ids::AccountId, ports::Clock,
///     store_fs::{StoppedStore, VerifyLimits, VerifyScratch}};
/// fn thaw(store: StoppedStore, clock: &dyn Clock, account: AccountId,
///     limits: VerifyLimits, scratch: VerifyScratch<'_>) {
///     let report = store.verify_account(&td_crypto::Provider, clock, account, limits, scratch).unwrap();
///     let root = store.into_locked();
///     let _ = report.identity();
///     drop(root);
/// }
/// ```
pub struct VerifiedAccount<'r> {
    _store: &'r StoppedStore,
    current: Current,
    identity: ViewIdentity,
    journal: Summary,
    physical_bytes: u64,
    incomplete_tail: bool,
    tables: CompleteTables,
    history: CompleteHistorySweep,
    references: reference_sweep::CompleteSweep,
    recipients: recipient_sweep::CompleteCoverage,
    mailboxes: mailbox_sweep::CompleteForest,
    blobs: CompleteBlobSweep,
}
impl VerifiedAccount<'_> {
    pub const fn current(&self) -> Current {
        self.current
    }
    pub const fn identity(&self) -> ViewIdentity {
        self.identity
    }
    pub const fn journal(&self) -> Summary {
        self.journal
    }
    pub const fn physical_journal_bytes(&self) -> u64 {
        self.physical_bytes
    }
    pub const fn has_incomplete_tail(&self) -> bool {
        self.incomplete_tail
    }
    pub const fn tables(&self) -> CompleteTables {
        self.tables
    }
    pub const fn history(&self) -> CompleteHistorySweep {
        self.history
    }
    pub const fn references(&self) -> reference_sweep::CompleteSweep {
        self.references
    }
    pub const fn recipients(&self) -> recipient_sweep::CompleteCoverage {
        self.recipients
    }
    pub const fn mailboxes(&self) -> mailbox_sweep::CompleteForest {
        self.mailboxes
    }
    pub const fn blobs(&self) -> CompleteBlobSweep {
        self.blobs
    }
}
/// Owns the stopped store and its verified single-account selection without scratch.
/// No writable root accessor, live read lease or service-readiness authority is exposed.
/// Consuming return discards the verification before restoring offline operations.
pub struct VerifiedStore {
    store: StoppedStore,
    current: Current,
    identity: ViewIdentity,
    journal: Summary,
}
impl VerifiedStore {
    pub const fn current(&self) -> Current {
        self.current
    }
    pub const fn identity(&self) -> ViewIdentity {
        self.identity
    }
    pub const fn journal(&self) -> Summary {
        self.journal
    }
    pub fn into_stopped(self) -> StoppedStore {
        self.store
    }
}
#[derive(Debug)]
pub enum OwnedVerifyError {
    Verification(VerifyError),
    IncompleteTail,
}
impl std::fmt::Display for OwnedVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Verification(_) => f.write_str("owned store verification failed"),
            Self::IncompleteTail => {
                f.write_str("verified store requires a complete active journal")
            }
        }
    }
}
impl std::error::Error for OwnedVerifyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Verification(e) => Some(e),
            Self::IncompleteTail => None,
        }
    }
}
impl StoppedStore {
    /// Consume exclusive store ownership and verify its one configured account.
    /// An incomplete tail refuses without repair. Any error drops this owner and
    /// releases its cooperative lock; a retry must reacquire and verify again.
    /// Full recovery accounting, mutation policy and activation remain separate.
    ///
    /// ```compile_fail,E0382
    /// use td_mta::{ids::AccountId, ports::Clock,
    ///     store_fs::{StoppedStore, VerifyLimits, VerifyScratch}};
    /// fn transfer(store: StoppedStore, clock: &dyn Clock, account: AccountId,
    ///     limits: VerifyLimits, scratch: VerifyScratch<'_>) {
    ///     let verified = store.verify_owned_account(&td_crypto::Provider, clock, account, limits, scratch).unwrap();
    ///     let root = store.into_locked();
    ///     drop((verified, root));
    /// }
    /// ```
    pub fn verify_owned_account<C: Crypto>(
        self,
        crypto: &C,
        clock: &dyn Clock,
        account: AccountId,
        limits: VerifyLimits,
        scratch: VerifyScratch<'_>,
    ) -> Result<VerifiedStore, OwnedVerifyError>
    where
        C::Sha256: Sync,
    {
        let report = self
            .verify_account(crypto, clock, account, limits, scratch)
            .map_err(OwnedVerifyError::Verification)?;
        if report.has_incomplete_tail() {
            return Err(OwnedVerifyError::IncompleteTail);
        }
        let current = report.current();
        let identity = report.identity();
        let journal = report.journal();
        Ok(VerifiedStore {
            store: self,
            current,
            identity,
            journal,
        })
    }
}
// All nested samplers share one monotonic watermark across phase boundaries.
struct VerifyClock<'a> {
    source: &'a dyn Clock,
    deadline: Deadline,
    last: AtomicU64,
}
impl Clock for VerifyClock<'_> {
    fn sample(&self) -> Result<Time, PolicyError> {
        let now = self.source.sample()?;
        let prior = self.last.fetch_max(now.monotonic.0, Ordering::Relaxed);
        if now.monotonic.0 < prior {
            return Err(PolicyError::Invalid);
        }
        if self.deadline.expired(now.monotonic) {
            return Err(PolicyError::Deadline);
        }
        Ok(now)
    }
}
impl VerifyClock<'_> {
    fn checked<T>(&self, work: impl FnOnce() -> Result<T, VerifyError>) -> Result<T, VerifyError> {
        self.sample().map_err(VerifyError::Policy)?;
        let result = work();
        self.sample().map_err(VerifyError::Policy).and(result)
    }
}
impl StoppedStore {
    /// Blocking offline verification; each bounded stage is bracketed by one deadline.
    /// Errors may overwrite all scratch. No files are repaired or published.
    pub fn verify_account<C: Crypto>(
        &self,
        crypto: &C,
        clock: &dyn Clock,
        account: AccountId,
        limits: VerifyLimits,
        scratch: VerifyScratch<'_>,
    ) -> Result<VerifiedAccount<'_>, VerifyError>
    where
        C::Sha256: Sync,
    {
        let clock = VerifyClock {
            source: clock,
            deadline: limits.deadline,
            last: AtomicU64::new(0),
        };
        let selection = clock.checked(|| {
            self.load_selection(crypto, account, scratch.selection)
                .map_err(VerifyError::Selection)
        })?;
        let mut capture = clock.checked(|| {
            self.capture_journal(crypto, selection, limits.capture_bytes, scratch.frame)
                .map_err(VerifyError::Capture)
        })?;
        while clock.checked(|| capture.advance().map_err(VerifyError::Capture))? != CaptureStep::End
        {
        }
        let captured = clock.checked(|| capture.finish().map_err(VerifyError::Capture))?;
        let identity = captured.identity();
        let journal = captured.summary();
        let physical_bytes = captured.physical_bytes();
        let incomplete_tail = captured.has_incomplete_tail();
        let active = clock.checked(|| {
            captured
                .load_overlay(
                    crypto,
                    limits.capture_bytes,
                    scratch.overlay_frames,
                    scratch.overlay_cells,
                )
                .map_err(VerifyError::Overlay)
        })?;
        drop(captured);
        let mut files = clock.checked(|| {
            self.validate_files(
                crypto,
                selection,
                &active,
                scratch.record,
                scratch.changes,
                limits.files,
            )
            .map_err(VerifyError::Files)
        })?;
        while !files.is_complete() {
            clock.checked(|| files.advance().map_err(VerifyError::Files))?;
        }
        let (files, record, changes) =
            clock.checked(|| files.finish().map_err(VerifyError::Files))?;
        let request = ValidationReadRequest {
            after: ChangeCursor {
                sequence: identity.history_floor,
                operation: u32::MAX,
            },
            kind: ObjectType::Email,
            deadline: limits.deadline,
            limits: limits.read,
        };
        let mut data = clock.checked(|| {
            files
                .validate_data(crypto, &clock, request, record, changes, limits.data)
                .map_err(VerifyError::Data)
        })?;
        while !data.is_complete() {
            clock.checked(|| {
                data.advance(scratch.key, scratch.value)
                    .map_err(VerifyError::Data)
            })?;
        }
        let data = clock.checked(|| data.finish().map_err(VerifyError::Data))?;
        let report = VerifiedAccount {
            _store: self,
            current: selection.current(),
            identity,
            journal,
            physical_bytes,
            incomplete_tail,
            tables: *files.tables(),
            history: *files.history(),
            references: data.references(),
            recipients: data.recipients(),
            mailboxes: data.mailboxes(),
            blobs: data.blobs(),
        };
        clock.sample().map_err(VerifyError::Policy)?;
        Ok(report)
    }
}

#[cfg(test)]
pub use tests::probe as probe_verify_account;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::super::{
        active::fixture, BlobInputError, BlobSweepError, HistorySweepLimits, TableSweepLimits,
    };
    use super::super::tests::prepare;
    use super::*;
    use crate::{
        format::{
            self,
            key::Key,
            operation::Operation,
            row::{BlobKind, BlobRow, Row},
            Sequence,
        },
        ids::BlobId,
        ports::{Digest, Tick},
        store_paths::{AccountEntry, Name, Number},
    };
    use td_crypto::Provider;
    const ACCOUNT: AccountId = AccountId::from_bytes([0x33; 16]);
    pub(super) struct Scratch {
        metadata: SelectionScratch,
        frame: Box<[u8; MAX_FRAME_BYTES]>,
        frames: [u8; 4096],
        cells: [overlay::Cell; 16],
        record: Box<[u8; MAX_RECORD_BYTES]>,
        changes: [frame_changes::Cell; 2],
        key: [u8; 1024],
        value: [u8; 1024],
    }
    impl Scratch {
        pub(super) fn new() -> Self {
            Self {
                metadata: SelectionScratch::new(),
                frame: vec![0; MAX_FRAME_BYTES]
                    .into_boxed_slice()
                    .try_into()
                    .unwrap(),
                frames: [0; 4096],
                cells: [overlay::Cell::EMPTY; 16],
                record: vec![0; MAX_RECORD_BYTES]
                    .into_boxed_slice()
                    .try_into()
                    .unwrap(),
                changes: [frame_changes::Cell::EMPTY; 2],
                key: [0; 1024],
                value: [0; 1024],
            }
        }
        fn borrowed(&mut self) -> VerifyScratch<'_> {
            VerifyScratch {
                selection: &mut self.metadata,
                frame: &mut self.frame,
                overlay_frames: &mut self.frames,
                overlay_cells: &mut self.cells,
                record: &mut self.record,
                changes: &mut self.changes,
                key: &mut self.key,
                value: &mut self.value,
            }
        }
        pub(super) fn journal(&mut self) -> JournalStartScratch<'_> {
            JournalStartScratch {
                selection: &mut self.metadata,
                frame: &mut self.frame,
            }
        }
        pub(super) fn read(&mut self) -> PinnedReadScratch<'_> {
            PinnedReadScratch {
                selection: &mut self.metadata,
                frames: &mut self.frames,
                cells: &mut self.cells,
                record: &mut self.record,
                changes: &mut self.changes,
            }
        }
        fn overwrite(&mut self) {
            self.metadata = SelectionScratch::new();
            self.frame.fill(0);
            self.frames.fill(0);
            self.cells.fill(overlay::Cell::EMPTY);
            self.record.fill(0);
            self.changes.fill(frame_changes::Cell::EMPTY);
            self.key.fill(0);
            self.value.fill(0);
        }
    }
    struct TestClock {
        calls: AtomicU64,
        fault_at: u64,
        mode: u8,
    }
    impl TestClock {
        fn new(fault_at: u64, mode: u8) -> Self {
            Self {
                calls: AtomicU64::new(0),
                fault_at,
                mode,
            }
        }
    }
    impl Clock for TestClock {
        fn sample(&self) -> Result<Time, PolicyError> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            if call >= self.fault_at && self.mode == 2 {
                return Err(PolicyError::Busy);
            }
            Ok(Time {
                utc_ms: 123,
                monotonic: Tick(if self.mode == 3 {
                    if call == self.fault_at {
                        1
                    } else {
                        2
                    }
                } else if call < self.fault_at {
                    2
                } else if self.mode == 1 {
                    1
                } else {
                    100
                }),
            })
        }
    }
    pub(super) fn limits() -> VerifyLimits {
        VerifyLimits {
            capture_bytes: 4096,
            files: ValidationLimits {
                tables: TableSweepLimits {
                    bytes: 1345,
                    rows: 1,
                },
                history: HistorySweepLimits {
                    bytes: 277,
                    frames: 1,
                },
            },
            read: ReadLimits {
                table_bytes: 225,
                change_source_bytes: 4096,
                steps: 128,
            },
            data: DataLimits {
                rows: 1,
                parent_reads: 0,
                blob_bytes: 3,
            },
            deadline: Deadline::after(Tick(0), 100).unwrap(),
        }
    }
    fn blob(store: &StoppedStore) {
        let id = BlobId::from_bytes([0x44; 16]);
        let mut digest = Provider.sha256().unwrap();
        digest.update(b"abc").unwrap();
        let row = Row::Blob(BlobRow {
            kind: BlobKind::Message,
            length: 3,
            digest: digest.finish().unwrap(),
            created_at: 0,
        });
        let mut value = [0; 1024];
        let n = row.encode(&mut value).unwrap();
        let mut key = [0; 16];
        Key::Blob(id).encode(&mut key).unwrap();
        let op = Operation::put(format::Table::Blobs, &key, &value[..n]).unwrap();
        let size = op.encoded_len().unwrap();
        let mut frame = vec![0; format::FRAME_HEADER_BYTES + size + format::FRAME_FOOTER_BYTES];
        op.encode(&mut frame[format::FRAME_HEADER_BYTES..format::FRAME_HEADER_BYTES + size])
            .unwrap();
        format::frame::seal(&Provider, Sequence::from_u64(3), 1, &mut frame).unwrap();
        let mut journal = fixture::journal();
        journal.extend_from_slice(&frame);
        fixture::write(&store.root, &journal);
        for entry in [
            AccountEntry::Messages,
            AccountEntry::Temporary,
            AccountEntry::Shard(BlobKind::Message, 0x44),
        ] {
            store.root.create_account_directory(ACCOUNT, entry).unwrap();
        }
        let mut file = store
            .root
            .create_temporary(ACCOUNT, Number::new(900).unwrap(), 3)
            .unwrap();
        file.write(b"abc").unwrap();
        file.sync()
            .unwrap()
            .publish_blob(BlobKind::Message, id)
            .unwrap();
    }
    #[test]
    fn actual_current_drives_empty_tail_and_blob_verification_with_reusable_scratch() {
        for mode in 0..3 {
            let (dir, store, _) = prepare();
            if mode == 0 {
                fixture::write(&store.root, &fixture::journal());
            }
            if mode == 2 {
                blob(&store);
            }
            let journal_path = dir.path.join(
                Name::account(ACCOUNT, AccountEntry::Journal(Number::new(2).unwrap()))
                    .unwrap()
                    .as_path()
                    .unwrap(),
            );
            let before = std::fs::read(&journal_path).unwrap();
            let mut scratch = Scratch::new();
            let clock = TestClock::new(u64::MAX, 0);
            let report = store
                .verify_account(&Provider, &clock, ACCOUNT, limits(), scratch.borrowed())
                .unwrap();
            scratch.overwrite();
            assert!(std::mem::size_of_val(&report) <= 2048);
            assert_eq!(report.current().generation, 2);
            assert_eq!(
                report.identity().committed_sequence.number(),
                if mode == 2 { 3 } else { 2 }
            );
            assert_eq!(report.identity().history_floor.number(), 0);
            assert_eq!(
                report.journal().through(),
                report.identity().committed_sequence
            );
            assert_eq!(report.physical_journal_bytes(), before.len() as u64);
            assert_eq!(report.has_incomplete_tail(), mode == 1);
            assert_eq!(report.tables().rows(), u64::from(mode == 2));
            assert_eq!(report.history().segments(), 1);
            assert_eq!(report.references().rows(), report.tables().rows());
            assert_eq!(report.references().utc_ms(), 123);
            assert_eq!(report.recipients().recipients(), 0);
            assert_eq!(report.mailboxes().mailboxes(), 0);
            assert_eq!(report.blobs().blobs(), u64::from(mode == 2));
            assert_eq!(report.blobs().bytes(), if mode == 2 { 3 } else { 0 });
            assert_eq!(std::fs::read(&journal_path).unwrap(), before);
        }
    }
    #[test]
    fn phase_failures_and_admission_never_return_partial_evidence() {
        for mode in 0..7 {
            let (dir, store, _) = prepare();
            let mut scratch = Scratch::new();
            let mut limits = limits();
            if mode == 0 {
                let path = Name::account(ACCOUNT, AccountEntry::Current).unwrap();
                std::fs::write(dir.path.join(path.as_path().unwrap()), b"bad current").unwrap();
            }
            if mode == 1 {
                let mut journal = fixture::journal();
                *journal.last_mut().unwrap() ^= 1;
                fixture::write(&store.root, &journal);
            }
            if mode == 2 {
                limits.capture_bytes = 255;
            }
            if mode == 4 {
                limits.files.history.bytes = 276;
            }
            if mode == 5 {
                fixture::write(&store.root, &fixture::journal()[..96]);
            }
            if mode == 6 {
                limits.read.steps = 1;
            }
            let mut buffers = scratch.borrowed();
            if mode == 3 {
                buffers.overlay_frames = &mut [];
            }
            let clock = TestClock::new(u64::MAX, 0);
            let result = store.verify_account(&Provider, &clock, ACCOUNT, limits, buffers);
            assert!(
                matches!(
                    (mode, result),
                    (0, Err(VerifyError::Selection(_)))
                        | (1 | 2, Err(VerifyError::Capture(_)))
                        | (3, Err(VerifyError::Overlay(_)))
                        | (4, Err(VerifyError::Files(_)))
                        | (5, Err(VerifyError::Data(DataError::Blobs(_))))
                        | (6, Err(VerifyError::Data(DataError::References(_))))
                ),
                "mode {mode}"
            );
        }
    }
    fn contains_policy(
        mut error: &(dyn std::error::Error + 'static),
        expected: PolicyError,
    ) -> bool {
        loop {
            if error.downcast_ref::<PolicyError>() == Some(&expected) {
                return true;
            }
            match error.source() {
                Some(source) => error = source,
                None => return false,
            }
        }
    }
    #[test]
    fn one_deadline_and_monotonic_watermark_cover_initial_nested_and_final_work() {
        let (_dir, store, _) = prepare();
        blob(&store);
        let mut scratch = Scratch::new();
        let clock = TestClock::new(u64::MAX, 0);
        let report = store
            .verify_account(&Provider, &clock, ACCOUNT, limits(), scratch.borrowed())
            .unwrap();
        let calls = clock.calls.load(Ordering::Relaxed);
        assert!(calls > 100);
        assert_eq!(report.blobs().bytes(), 3);
        for mode in 0..3 {
            for at in [1, 5, 10, 40, calls / 2, calls - 2, calls - 1] {
                let clock = TestClock::new(at, mode);
                let result =
                    store.verify_account(&Provider, &clock, ACCOUNT, limits(), scratch.borrowed());
                let expected = match mode {
                    0 => PolicyError::Deadline,
                    1 => PolicyError::Invalid,
                    _ => PolicyError::Busy,
                };
                assert!(
                    matches!(result, Err(VerifyError::Policy(error)) if error == expected),
                    "mode {mode} at {at}"
                );
            }
        }
        // A single recovered regression can otherwise hide at a nested view's first sample.
        for at in 1..calls {
            let clock = TestClock::new(at, 3);
            let result =
                store.verify_account(&Provider, &clock, ACCOUNT, limits(), scratch.borrowed());
            assert!(
                matches!(result, Err(ref error) if contains_policy(error, PolicyError::Invalid)),
                "transient regression at {at}"
            );
        }
        let clock = TestClock::new(0, 0);
        assert!(matches!(
            store.verify_account(&Provider, &clock, ACCOUNT, limits(), scratch.borrowed()),
            Err(VerifyError::Policy(PolicyError::Deadline))
        ));
        assert_eq!(clock.calls.load(Ordering::Relaxed), 1);
    }
    pub(super) fn owned_fixture() -> (super::super::super::tests::Fixture, VerifiedStore, Scratch) {
        owned_fixture_with(super::super::super::tests::Fixture::new())
    }
    pub(super) fn owned_fixture_with(
        dir: super::super::super::tests::Fixture,
    ) -> (super::super::super::tests::Fixture, VerifiedStore, Scratch) {
        let (dir, store, _) = super::super::tests::prepare_fixture(dir);
        fixture::write(&store.root, &fixture::journal());
        let mut scratch = Scratch::new();
        let clock = TestClock::new(u64::MAX, 0);
        let verified = store
            .verify_owned_account(&Provider, &clock, ACCOUNT, limits(), scratch.borrowed())
            .unwrap();
        (dir, verified, scratch)
    }
    #[test]
    fn owned_verification_keeps_exclusion_releases_scratch_and_consumes_its_proof() {
        use super::super::super::{acquire_lock, Directory, LockError};
        use std::os::unix::fs::MetadataExt;
        let (dir, store, _) = prepare();
        blob(&store);
        let mut scratch = Scratch::new();
        let clock = TestClock::new(u64::MAX, 0);
        let report = store
            .verify_account(&Provider, &clock, ACCOUNT, limits(), scratch.borrowed())
            .unwrap();
        let expected = (report.current(), report.identity(), report.journal());
        let directory = Directory::from_path(dir.path.to_str().unwrap()).unwrap();
        let owner = directory.metadata().unwrap().uid();
        let verified = store
            .verify_owned_account(&Provider, &clock, ACCOUNT, limits(), scratch.borrowed())
            .unwrap();
        scratch.overwrite();
        assert_eq!(
            (verified.current(), verified.identity(), verified.journal()),
            expected
        );
        assert!(matches!(
            acquire_lock(&directory, owner),
            Err(LockError::Busy)
        ));
        assert!(std::mem::size_of::<VerifiedStore>() <= 2048);
        let store = verified.into_stopped();
        assert!(matches!(
            acquire_lock(&directory, owner),
            Err(LockError::Busy)
        ));
        let report = store
            .verify_account(&Provider, &clock, ACCOUNT, limits(), scratch.borrowed())
            .unwrap();
        assert_eq!(
            (report.current(), report.identity(), report.journal()),
            expected
        );
        drop(store);
        drop(dir.reacquire());
    }
    #[test]
    fn owned_verification_refuses_tail_or_failed_validation_without_changing_files() {
        for mode in 0..3 {
            let (dir, store, _) = prepare();
            if mode != 0 {
                blob(&store);
            }
            let entry = if mode == 2 {
                AccountEntry::Current
            } else {
                AccountEntry::Journal(Number::new(2).unwrap())
            };
            let path = dir
                .path
                .join(Name::account(ACCOUNT, entry).unwrap().as_path().unwrap());
            if mode == 2 {
                std::fs::write(&path, [0; format::CURRENT_BYTES]).unwrap();
            }
            let before = std::fs::read(&path).unwrap();
            let mut scratch = Scratch::new();
            let clock = TestClock::new(if mode == 1 { 0 } else { u64::MAX }, 0);
            let result = store.verify_owned_account(
                &Provider,
                &clock,
                ACCOUNT,
                limits(),
                scratch.borrowed(),
            );
            match mode {
                0 => assert!(matches!(result, Err(OwnedVerifyError::IncompleteTail))),
                1 => assert!(matches!(
                    result,
                    Err(OwnedVerifyError::Verification(VerifyError::Policy(
                        PolicyError::Deadline
                    )))
                )),
                _ => assert!(matches!(
                    result,
                    Err(OwnedVerifyError::Verification(VerifyError::Selection(_)))
                )),
            }
            assert_eq!(std::fs::read(&path).unwrap(), before);
            drop(dir.reacquire());
        }
    }
    /// Only calls the observer around verification; fixture I/O and allocation are cold.
    pub fn probe(mut snapshot: impl FnMut()) {
        use super::super::super::tests::Fixture;
        for maximum in [false, true] {
            for populated in [false, true] {
                let fixture = if maximum {
                    Fixture::maximum_root()
                } else {
                    Fixture::new()
                };
                if maximum {
                    assert_eq!(
                        fixture.path.as_os_str().len(),
                        super::super::super::MAX_ROOT_BYTES
                    );
                }
                let (dir, store, _) = super::super::tests::prepare_fixture(fixture);
                if populated {
                    blob(&store);
                }
                let mut scratch = Scratch::new();
                let clock = TestClock::new(u64::MAX, 0);
                let expired = TestClock::new(0, 0);
                let mid_deadline = TestClock::new(40, 0);
                snapshot();
                {
                    let report = store
                        .verify_account(&Provider, &clock, ACCOUNT, limits(), scratch.borrowed())
                        .unwrap();
                    scratch.overwrite();
                    assert_eq!(report.blobs().bytes(), if populated { 3 } else { 0 });
                    assert_eq!(report.has_incomplete_tail(), !populated);
                }
                assert!(matches!(
                    store.verify_account(
                        &Provider,
                        &expired,
                        ACCOUNT,
                        limits(),
                        scratch.borrowed()
                    ),
                    Err(VerifyError::Policy(PolicyError::Deadline))
                ));
                assert!(matches!(
                    store.verify_account(
                        &Provider,
                        &mid_deadline,
                        ACCOUNT,
                        limits(),
                        scratch.borrowed()
                    ),
                    Err(VerifyError::Policy(PolicyError::Deadline))
                ));
                let mut short = limits();
                short.capture_bytes = 255;
                assert!(matches!(
                    store.verify_account(&Provider, &clock, ACCOUNT, short, scratch.borrowed()),
                    Err(VerifyError::Capture(_))
                ));
                let mut buffers = scratch.borrowed();
                buffers.overlay_frames = &mut [];
                assert!(matches!(
                    store.verify_account(&Provider, &clock, ACCOUNT, limits(), buffers),
                    Err(VerifyError::Overlay(_))
                ));
                let mut short = limits();
                short.files.history.bytes = 276;
                assert!(matches!(
                    store.verify_account(&Provider, &clock, ACCOUNT, short, scratch.borrowed()),
                    Err(VerifyError::Files(_))
                ));
                let mut short = limits();
                short.read.steps = 1;
                assert!(matches!(
                    store.verify_account(&Provider, &clock, ACCOUNT, short, scratch.borrowed()),
                    Err(VerifyError::Data(DataError::References(_)))
                ));
                snapshot();
                // Persisted corruption is prepared outside the measurement interval.
                let name = Name::account(
                    ACCOUNT,
                    if populated {
                        AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([0x44; 16]))
                    } else {
                        AccountEntry::Current
                    },
                )
                .unwrap();
                std::fs::write(
                    dir.path.join(name.as_path().unwrap()),
                    if populated { b"abd" } else { b"bad" },
                )
                .unwrap();
                snapshot();
                let result =
                    store.verify_account(&Provider, &clock, ACCOUNT, limits(), scratch.borrowed());
                assert!(matches!(
                    (populated, result),
                    (false, Err(VerifyError::Selection(_)))
                        | (
                            true,
                            Err(VerifyError::Data(DataError::Blobs(BlobSweepError::Input(
                                BlobInputError::Checksum
                            ))))
                        )
                ));
                snapshot();
            }
        }
    }
}
