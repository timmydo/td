//! Scoped single-generation ownership with serialized append and paired visibility.
use super::super::super::{
    AppendStep, ReconciledAppend, RecoveryInputError, ReservedAppend, ReservedAppendError,
    ScannedJournal, SelectionError,
};
use super::{SelectionScratch, VerifiedStore, VerifyClock};
use crate::{
    admission::{
        quota::Kind,
        writer::{self, FrameBudget, WriterLedger},
    },
    format::{JOURNAL_HEADER_BYTES, MAX_FRAME_BYTES},
    ports::{Clock, Crypto, Deadline, Error as PolicyError, ViewIdentity},
};
use std::sync::{atomic::AtomicU64, Mutex, TryLockError};

#[derive(Clone, Copy, Debug)]
pub struct JournalStart {
    pub max_bytes: u64,
    pub deadline: Deadline,
    /// Must match the caller's admitted read-view capacity (one through eight).
    pub views: u8,
}
pub struct JournalStartScratch<'a> {
    pub selection: &'a mut SelectionScratch,
    pub frame: &'a mut [u8; MAX_FRAME_BYTES],
}
#[derive(Debug)]
pub enum JournalError {
    Policy(PolicyError),
    Selection(SelectionError),
    Scan(RecoveryInputError),
    Append(ReservedAppendError),
    Ledger(writer::Error),
}
impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("journal session failed")
    }
}
impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Policy(e) => Some(e),
            Self::Selection(e) => Some(e),
            Self::Scan(e) => Some(e),
            Self::Append(e) => Some(e),
            Self::Ledger(e) => Some(e),
        }
    }
}
#[derive(Debug)]
pub enum CommitError {
    /// No append started; this refusal does not retire an otherwise healthy writer.
    Rejected(JournalError),
    /// This session cannot write again. Bytes or visibility may already have changed.
    Stopped(JournalError),
}
impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Rejected(_) => "journal commit rejected before append",
            Self::Stopped(_) => "journal writer stopped; recovery required",
        })
    }
}
impl std::error::Error for CommitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Rejected(e) | Self::Stopped(e) => e,
        })
    }
}
enum Boundary<'r> {
    Scanned(ScannedJournal<'r>),
    Appended(ReconciledAppend<'r>),
}
impl<'r> Boundary<'r> {
    fn append<'b, 'l, 'a>(
        self,
        crypto: &impl Crypto,
        bytes: &'b [u8],
        ledger: &'l mut WriterLedger<'a>,
        frame: writer::FrameId,
        now: crate::ports::Tick,
    ) -> Result<ReservedAppend<'r, 'b, 'l, 'a>, ReservedAppendError> {
        match self {
            Self::Scanned(scan) => scan.append_reserved(crypto, bytes, ledger, frame, now),
            Self::Appended(prior) => prior.append_reserved(crypto, bytes, ledger, frame, now),
        }
    }
}
struct Writer<'r, 'a> {
    boundary: Option<Boundary<'r>>,
    ledger: WriterLedger<'a>,
}
struct Published {
    identity: ViewIdentity,
    readers: u8,
    invalid_reader_count: bool,
}
/// All owners stay in the consuming scope; no namespace mutation or raw handle escapes.
pub struct JournalSession<'r, 'a> {
    store: &'r VerifiedStore,
    writer: Mutex<Writer<'r, 'a>>,
    published: Mutex<Published>,
    views: u8,
}
/// Pins this session's generation, journal and history until drop.
/// The identity is an inspection copy, not a standalone pin or authorization.
pub struct CommittedView<'v, 'r, 'a> {
    session: &'v JournalSession<'r, 'a>,
    identity: ViewIdentity,
}
impl CommittedView<'_, '_, '_> {
    pub const fn identity(&self) -> ViewIdentity {
        self.identity
    }
}
impl Drop for CommittedView<'_, '_, '_> {
    fn drop(&mut self) {
        let mut state = match self.session.published.lock() {
            Ok(state) => state,
            Err(error) => error.into_inner(),
        };
        match state.readers.checked_sub(1) {
            Some(n) => state.readers = n,
            None => state.invalid_reader_count = true,
        }
    }
}
fn policy(error: PolicyError) -> JournalError {
    JournalError::Policy(error)
}
fn lock_error<T>(error: TryLockError<T>) -> JournalError {
    policy(match error {
        TryLockError::WouldBlock => PolicyError::Busy,
        TryLockError::Poisoned(_) => PolicyError::WriterStopped,
    })
}
fn checked<T>(
    clock: &VerifyClock<'_>,
    work: impl FnOnce() -> Result<T, JournalError>,
) -> Result<T, JournalError> {
    clock.sample().map_err(policy)?;
    let result = work();
    clock.sample().map_err(policy)?;
    result
}
impl VerifiedStore {
    /// Consume the verified owner and recovered ledger for this entire callback.
    /// Recheck CURRENT and the exact journal digest before lending a session.
    /// Scratch is cold; the recovery frame is returned to the callback for reuse.
    /// The run callback starts without either mutex held. Scoped threads may share it.
    ///
    /// ```compile_fail
    /// use td_mta::{admission::writer::WriterLedger, ports::Clock,
    ///     store_fs::{VerifiedStore, JournalStart, JournalStartScratch}};
    /// fn escape(store: VerifiedStore, ledger: WriterLedger<'_>, clock: &dyn Clock,
    ///     limits: JournalStart, scratch: JournalStartScratch<'_>) {
    ///     // A borrowed pin cannot outlive the callback's owned store/session.
    ///     let view = store.with_journal(&td_crypto::Provider, clock, ledger, limits, scratch,
    ///         |session, _| session.capture().unwrap()).unwrap();
    ///     let _ = view.identity();
    /// }
    /// ```
    pub fn with_journal<C: Crypto, R>(
        self,
        crypto: &C,
        clock: &dyn Clock,
        ledger: WriterLedger<'_>,
        limits: JournalStart,
        scratch: JournalStartScratch<'_>,
        run: impl FnOnce(&JournalSession<'_, '_>, &mut [u8; MAX_FRAME_BYTES]) -> R,
    ) -> Result<R, JournalError> {
        if !(1..=8).contains(&limits.views) {
            return Err(policy(PolicyError::Invalid));
        }
        let clock = VerifyClock {
            source: clock,
            deadline: limits.deadline,
            last: AtomicU64::new(0),
        };
        let selection = checked(&clock, || {
            self.store
                .root
                .load_selection(crypto, self.identity.account, scratch.selection)
                .map_err(JournalError::Selection)
        })?;
        if selection.current() != self.current {
            return Err(policy(PolicyError::Corrupt));
        }
        let mut scan = checked(&clock, || {
            self.store
                .root
                .scan_active_journal(crypto, selection, limits.max_bytes, scratch.frame)
                .map_err(JournalError::Scan)
        })?;
        while checked(&clock, || {
            scan.next_frame()
                .map(|f| f.is_some())
                .map_err(JournalError::Scan)
        })? {}
        let scan = checked(&clock, || scan.finish().map_err(JournalError::Scan))?;
        if scan.has_incomplete_tail()
            || scan.summary() != self.journal
            || scan.valid_bytes() != self.identity.committed_offset
        {
            return Err(policy(PolicyError::Corrupt));
        }
        if ledger.phase() != writer::Phase::Open {
            return Err(policy(PolicyError::WriterStopped));
        }
        if ledger
            .used(Kind::ActiveJournalBytes)
            .map_err(JournalError::Ledger)?
            != self
                .identity
                .committed_offset
                .checked_sub(JOURNAL_HEADER_BYTES as u64)
                .ok_or_else(|| policy(PolicyError::Corrupt))?
            || ledger
                .used(Kind::ActiveJournalOperations)
                .map_err(JournalError::Ledger)?
                != u64::try_from(self.journal.operations())
                    .map_err(|_| policy(PolicyError::Corrupt))?
        {
            return Err(policy(PolicyError::Invalid));
        }
        let session = JournalSession {
            store: &self,
            writer: Mutex::new(Writer {
                boundary: Some(Boundary::Scanned(scan)),
                ledger,
            }),
            published: Mutex::new(Published {
                identity: self.identity,
                readers: 0,
                invalid_reader_count: false,
            }),
            views: limits.views,
        };
        clock.sample().map_err(policy)?;
        Ok(run(&session, scratch.frame))
    }
}
impl<'r, 'a> JournalSession<'r, 'a> {
    /// Reserve one admitted reader slot and capture the whole identity under one lock.
    pub fn capture(&self) -> Result<CommittedView<'_, 'r, 'a>, JournalError> {
        let mut state = self.published.try_lock().map_err(lock_error)?;
        if state.invalid_reader_count {
            return Err(policy(PolicyError::WriterStopped));
        }
        if state.readers >= self.views {
            return Err(policy(PolicyError::Capacity));
        }
        state.readers = state
            .readers
            .checked_add(1)
            .ok_or_else(|| policy(PolicyError::Capacity))?;
        Ok(CommittedView {
            session: self,
            identity: state.identity,
        })
    }
    /// Append one admitted immutable frame. Full transaction/blob policy remains external.
    /// A stopped result forbids further writes, even if its new identity became visible.
    pub fn commit(
        &self,
        crypto: &impl Crypto,
        clock: &dyn Clock,
        deadline: Deadline,
        bytes: &[u8],
        budget: FrameBudget,
    ) -> Result<ViewIdentity, CommitError> {
        let clock = VerifyClock {
            source: clock,
            deadline,
            last: AtomicU64::new(0),
        };
        let now = clock
            .sample()
            .map_err(|e| CommitError::Rejected(policy(e)))?
            .monotonic;
        let mut writer = self.writer.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => CommitError::Rejected(policy(PolicyError::Busy)),
            TryLockError::Poisoned(_) => CommitError::Stopped(policy(PolicyError::WriterStopped)),
        })?;
        if writer.boundary.is_none() {
            return Err(CommitError::Stopped(policy(PolicyError::WriterStopped)));
        }
        let grant = writer
            .ledger
            .prepare(&[], Some(budget), deadline, now)
            .and_then(|prepared| prepared.install(now))
            .map_err(|e| CommitError::Rejected(JournalError::Ledger(e)))?;
        // Absence is sticky on every return or unwind after reservation installation.
        let boundary = writer
            .boundary
            .take()
            .ok_or_else(|| CommitError::Stopped(policy(PolicyError::WriterStopped)))?;
        let result = (|| {
            let frame = grant.frame().ok_or_else(|| policy(PolicyError::Invalid))?;
            let now = clock.sample().map_err(policy)?.monotonic;
            let mut append = checked(&clock, || {
                boundary
                    .append(crypto, bytes, &mut writer.ledger, frame, now)
                    .map_err(JournalError::Append)
            })?;
            loop {
                if checked(&clock, || append.advance().map_err(JournalError::Append))?
                    == AppendStep::Complete
                {
                    break;
                }
            }
            let receipt = checked(&clock, || append.finish().map_err(JournalError::Append))?;
            writer.ledger.cancel(grant).map_err(JournalError::Ledger)?;
            let mut state = self
                .published
                .lock()
                .map_err(|_| policy(PolicyError::WriterStopped))?;
            let durable = receipt.durable();
            if state.invalid_reader_count
                || durable.current() != self.store.current
                || state
                    .identity
                    .committed_sequence
                    .successor()
                    .map_err(|_| policy(PolicyError::Corrupt))?
                    != durable.through()
                || state
                    .identity
                    .committed_offset
                    .checked_add(durable.frame_bytes() as u64)
                    != Some(durable.end())
            {
                return Err(policy(PolicyError::Corrupt));
            }
            let identity = ViewIdentity {
                committed_sequence: durable.through(),
                committed_offset: durable.end(),
                ..state.identity
            };
            state.identity = identity;
            drop(state);
            // A late timeout may have published; it still never acknowledges success.
            clock.sample().map_err(policy)?;
            Ok((receipt, identity))
        })();
        match result {
            Ok((receipt, identity)) => {
                writer.boundary = Some(Boundary::Appended(receipt));
                Ok(identity)
            }
            Err(e) => Err(CommitError::Stopped(e)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::super::tests::owned_fixture;
    use super::*;
    use crate::{
        admission::{logical::Cell, quota::Usage, DiskLimits, ViewMode, WorkLimits},
        format::{self, Sequence},
        limits::Limits,
        ownership::SlotState,
        ports::{Tick, Time},
        store_paths::{AccountEntry, Name, Number},
    };
    use std::sync::atomic::Ordering;
    use td_crypto::Provider;

    struct TestClock {
        calls: AtomicU64,
        fault: u64,
    }
    impl TestClock {
        fn new(fault: u64) -> Self {
            Self {
                calls: AtomicU64::new(0),
                fault,
            }
        }
    }
    impl Clock for TestClock {
        fn sample(&self) -> Result<Time, PolicyError> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(if call >= self.fault { 100 } else { 2 }),
            })
        }
    }
    trait CommitFixture {
        fn commit_fixture(
            &self,
            crypto: &Provider,
            clock: &TestClock,
            deadline: Deadline,
            bytes: &[u8],
            budget: FrameBudget,
        ) -> Result<ViewIdentity, CommitError>;
    }
    impl CommitFixture for JournalSession<'_, '_> {
        fn commit_fixture(
            &self,
            crypto: &Provider,
            clock: &TestClock,
            deadline: Deadline,
            bytes: &[u8],
            budget: FrameBudget,
        ) -> Result<ViewIdentity, CommitError> {
            for _ in 0..1000 {
                clock.calls.store(0, Ordering::Relaxed);
                match self.commit(crypto, clock, deadline, bytes, budget) {
                    Err(CommitError::Rejected(JournalError::Ledger(writer::Error::Logical(
                        crate::admission::logical::Error::Slot(crate::ownership::Error::Contended),
                    )))) => std::thread::yield_now(),
                    result => return result,
                }
            }
            panic!("unrelated ticket issuance never released admission");
        }
    }
    fn deadline() -> Deadline {
        Deadline::after(Tick(0), 100).unwrap()
    }
    fn start() -> JournalStart {
        JournalStart {
            max_bytes: 4096,
            deadline: deadline(),
            views: 2,
        }
    }
    fn budget() -> FrameBudget {
        FrameBudget::new(160, 2).unwrap()
    }
    fn frame(sequence: u64) -> Vec<u8> {
        let mut bytes = super::super::super::super::active::fixture::hex(include_str!(
            "../../../../tests/fixtures/format-v1/frame-delete-change.hex"
        ));
        format::frame::seal(&Provider, Sequence::from_u64(sequence), 2, &mut bytes).unwrap();
        bytes
    }
    fn with_ledger(run: impl FnOnce(WriterLedger<'_>)) {
        with_ledger_usage(160, 2, run);
    }
    fn with_ledger_usage(bytes: u64, operations: u64, run: impl FnOnce(WriterLedger<'_>)) {
        let plan = DiskLimits::default()
            .plan(
                &Limits::default().plan().unwrap(),
                WorkLimits::default(),
                ViewMode::OnlineBackground,
            )
            .unwrap();
        let mut used = Usage::default();
        used.add(Kind::ActiveJournalBytes, bytes).unwrap();
        used.add(Kind::ActiveJournalOperations, operations).unwrap();
        used.add(Kind::LiveMetadataBytes, 1232).unwrap();
        let mut states = [const { SlotState::EMPTY }; 8];
        let mut cells = [const { Cell::EMPTY }; 8];
        run(WriterLedger::new(&plan, 1232, used, &mut states, &mut cells).unwrap());
    }
    #[test]
    fn success_publishes_pairs_reuses_reader_slots_and_retains_old_pins() {
        with_ledger(|ledger| {
            let (dir, verified, mut scratch) = owned_fixture();
            let initial = verified.identity();
            let clock = TestClock::new(u64::MAX);
            verified
                .with_journal(
                    &Provider,
                    &clock,
                    ledger,
                    start(),
                    scratch.journal(),
                    |session, frame_scratch| {
                        frame_scratch.fill(0);
                        assert!(std::mem::size_of::<JournalSession<'_, '_>>() <= 8192);
                        let old = session.capture().unwrap();
                        let other = session.capture().unwrap();
                        assert!(matches!(
                            session.capture(),
                            Err(JournalError::Policy(PolicyError::Capacity))
                        ));
                        drop(other);
                        for sequence in 3..=6 {
                            let identity = session
                                .commit_fixture(
                                    &Provider,
                                    &clock,
                                    deadline(),
                                    &frame(sequence),
                                    budget(),
                                )
                                .unwrap();
                            assert_eq!(identity.committed_sequence, Sequence::from_u64(sequence));
                            assert_eq!(identity.committed_offset, 96 + (sequence - 1) * 160);
                            assert_eq!(old.identity(), initial);
                            let new = session.capture().unwrap();
                            assert_eq!(new.identity(), identity);
                            let writer = session.writer.lock().unwrap();
                            assert_eq!(
                                writer.ledger.used(Kind::ActiveJournalBytes).unwrap(),
                                (sequence - 1) * 160
                            );
                            assert_eq!(writer.ledger.pending(Kind::ActiveJournalBytes).unwrap(), 0);
                        }
                    },
                )
                .unwrap();
            drop(dir.reacquire());
        });
    }
    #[test]
    fn each_commit_clock_boundary_has_exact_visibility_and_retirement() {
        for fault in 0..13 {
            with_ledger(|ledger| {
                let (dir, verified, mut scratch) = owned_fixture();
                let initial = verified.identity();
                let path = dir.path.join(
                    Name::account(
                        initial.account,
                        AccountEntry::Journal(Number::new(2).unwrap()),
                    )
                    .unwrap()
                    .as_path()
                    .unwrap(),
                );
                let clock = TestClock::new(u64::MAX);
                verified
                    .with_journal(
                        &Provider,
                        &clock,
                        ledger,
                        start(),
                        scratch.journal(),
                        |session, _| {
                            let old = session.capture().unwrap();
                            let result = session.commit_fixture(
                                &Provider,
                                &TestClock::new(fault),
                                deadline(),
                                &frame(3),
                                budget(),
                            );
                            if fault == 0 {
                                assert!(matches!(
                                    result,
                                    Err(CommitError::Rejected(JournalError::Policy(
                                        PolicyError::Deadline
                                    )))
                                ));
                            } else {
                                assert!(matches!(
                                    result,
                                    Err(CommitError::Stopped(JournalError::Policy(
                                        PolicyError::Deadline
                                    )))
                                ));
                            }
                            let new = session.capture().unwrap();
                            assert_eq!(old.identity(), initial);
                            assert_eq!(
                                new.identity().committed_sequence,
                                Sequence::from_u64(if fault == 12 { 3 } else { 2 })
                            );
                            assert_eq!(
                                new.identity().committed_offset,
                                if fault == 12 { 416 } else { 256 }
                            );
                            assert_eq!(
                                std::fs::metadata(&path).unwrap().len(),
                                if fault >= 5 { 416 } else { 256 }
                            );
                            let writer = session.writer.lock().unwrap();
                            assert_eq!(writer.boundary.is_some(), fault == 0);
                            assert_eq!(
                                writer.ledger.used(Kind::ActiveJournalBytes).unwrap(),
                                if fault >= 11 { 320 } else { 160 }
                            );
                            assert_eq!(
                                writer.ledger.pending(Kind::ActiveJournalBytes).unwrap(),
                                if (1..11).contains(&fault) { 160 } else { 0 }
                            );
                            drop(writer);
                            let again = session.commit_fixture(
                                &Provider,
                                &clock,
                                deadline(),
                                &frame(3),
                                budget(),
                            );
                            if fault == 0 {
                                assert!(again.is_ok());
                            } else {
                                assert!(matches!(
                                    again,
                                    Err(CommitError::Stopped(JournalError::Policy(
                                        PolicyError::WriterStopped
                                    )))
                                ));
                            }
                        },
                    )
                    .unwrap();
                drop(dir.reacquire());
            });
        }
    }
    #[test]
    fn startup_checks_digest_selection_capacity_deadline_and_ledger_before_callback() {
        for mode in 0..9 {
            with_ledger_usage(
                if mode == 7 { 320 } else { 160 },
                if mode == 8 { 4 } else { 2 },
                |mut ledger| {
                    let (dir, verified, mut scratch) = owned_fixture();
                    let account = verified.identity().account;
                    let mut limits = start();
                    let clock = TestClock::new(if mode == 0 { 0 } else { u64::MAX });
                    match mode {
                        1 => limits.views = 0,
                        2 => limits.max_bytes = 255,
                        3 => {
                            // Mutate physical metadata only as an adversarial external fixture.
                            let path = dir.path.join(
                                Name::account(account, AccountEntry::Current)
                                    .unwrap()
                                    .as_path()
                                    .unwrap(),
                            );
                            std::fs::write(path, [0; format::CURRENT_BYTES]).unwrap();
                        }
                        4 => {
                            let mut bytes = super::super::super::super::active::fixture::journal();
                            let mut changed = frame(2);
                            changed[format::FRAME_HEADER_BYTES + 20] ^= 1;
                            format::frame::seal(&Provider, Sequence::from_u64(2), 2, &mut changed)
                                .unwrap();
                            bytes.truncate(format::JOURNAL_HEADER_BYTES);
                            bytes.extend_from_slice(&changed);
                            let path = dir.path.join(
                                Name::account(
                                    account,
                                    AccountEntry::Journal(Number::new(2).unwrap()),
                                )
                                .unwrap()
                                .as_path()
                                .unwrap(),
                            );
                            std::fs::write(path, bytes).unwrap();
                        }
                        5 => {
                            let checkpoint = ledger.begin_checkpoint().unwrap();
                            checkpoint.uncertain();
                        }
                        6 => {
                            let path = dir.path.join(
                                Name::account(
                                    account,
                                    AccountEntry::Journal(Number::new(2).unwrap()),
                                )
                                .unwrap()
                                .as_path()
                                .unwrap(),
                            );
                            let mut file =
                                std::fs::OpenOptions::new().append(true).open(path).unwrap();
                            std::io::Write::write_all(&mut file, b"x").unwrap();
                        }
                        _ => (),
                    }
                    let result = verified.with_journal(
                        &Provider,
                        &clock,
                        ledger,
                        limits,
                        scratch.journal(),
                        |_, _| panic!("invalid startup reached callback"),
                    );
                    match mode {
                        0 => assert!(matches!(
                            result,
                            Err(JournalError::Policy(PolicyError::Deadline))
                        )),
                        1 => assert!(matches!(
                            result,
                            Err(JournalError::Policy(PolicyError::Invalid))
                        )),
                        2 => assert!(matches!(result, Err(JournalError::Scan(_)))),
                        3 => assert!(matches!(result, Err(JournalError::Selection(_)))),
                        5 => assert!(matches!(
                            result,
                            Err(JournalError::Policy(PolicyError::WriterStopped))
                        )),
                        7 | 8 => assert!(matches!(
                            result,
                            Err(JournalError::Policy(PolicyError::Invalid))
                        )),
                        _ => assert!(matches!(
                            result,
                            Err(JournalError::Policy(PolicyError::Corrupt))
                        )),
                    }
                    drop(dir.reacquire());
                },
            );
        }
    }
    #[test]
    fn concurrent_captures_never_mix_sequence_and_extent() {
        with_ledger(|ledger| {
            let (_dir, verified, mut scratch) = owned_fixture();
            let clock = TestClock::new(u64::MAX);
            verified
                .with_journal(
                    &Provider,
                    &clock,
                    ledger,
                    start(),
                    scratch.journal(),
                    |session, _| {
                        std::thread::scope(|scope| {
                            let (updates, receive_update) = std::sync::mpsc::sync_channel(0);
                            let (seen, receive_seen) = std::sync::mpsc::sync_channel(0);
                            let reader = scope.spawn(move || {
                                for expected in 3..=20 {
                                    for _ in 0..64 {
                                        match session.capture() {
                                            Ok(view) => {
                                                let value = view.identity();
                                                assert_eq!(
                                                    value.committed_offset,
                                                    96 + (value.committed_sequence.number() - 1)
                                                        * 160
                                                );
                                            }
                                            Err(JournalError::Policy(PolicyError::Busy)) => (),
                                            Err(e) => panic!("unexpected capture error: {e:?}"),
                                        }
                                        std::thread::yield_now();
                                    }
                                    assert_eq!(
                                        receive_update
                                            .recv_timeout(std::time::Duration::from_secs(10))
                                            .unwrap(),
                                        expected
                                    );
                                    let view = session.capture().unwrap();
                                    assert_eq!(
                                        view.identity().committed_sequence,
                                        Sequence::from_u64(expected)
                                    );
                                    assert_eq!(
                                        view.identity().committed_offset,
                                        96 + (expected - 1) * 160
                                    );
                                    drop(view);
                                    seen.send(()).unwrap();
                                }
                            });
                            for sequence in 3..=20 {
                                session
                                    .commit_fixture(
                                        &Provider,
                                        &clock,
                                        deadline(),
                                        &frame(sequence),
                                        budget(),
                                    )
                                    .unwrap();
                                updates.send(sequence).unwrap();
                                receive_seen
                                    .recv_timeout(std::time::Duration::from_secs(10))
                                    .unwrap();
                            }
                            reader.join().unwrap();
                        });
                    },
                )
                .unwrap();
        });
    }
    #[test]
    fn contention_poisoning_and_constructor_refusals_do_not_acknowledge() {
        for mode in 0..6 {
            with_ledger(|ledger| {
                let (_dir, verified, mut scratch) = owned_fixture();
                let clock = TestClock::new(u64::MAX);
                verified
                    .with_journal(
                        &Provider,
                        &clock,
                        ledger,
                        start(),
                        scratch.journal(),
                        |session, _| {
                            let old = session.capture().unwrap();
                            let initial = old.identity();
                            let mut bytes = frame(3);
                            if mode == 0 {
                                let guard = session.writer.lock().unwrap();
                                assert!(matches!(
                                    session.commit_fixture(
                                        &Provider,
                                        &clock,
                                        deadline(),
                                        &bytes,
                                        budget()
                                    ),
                                    Err(CommitError::Rejected(JournalError::Policy(
                                        PolicyError::Busy
                                    )))
                                ));
                                drop(guard);
                            } else if mode == 1 {
                                let guard = session.published.lock().unwrap();
                                assert!(matches!(
                                    session.capture(),
                                    Err(JournalError::Policy(PolicyError::Busy))
                                ));
                                drop(guard);
                            } else if mode == 2 || mode == 3 {
                                let result =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        if mode == 2 {
                                            let _guard = session.writer.lock().unwrap();
                                            panic!("writer poison fixture");
                                        } else {
                                            let _guard = session.published.lock().unwrap();
                                            panic!("publication poison fixture");
                                        }
                                    }));
                                assert!(result.is_err());
                            } else if mode == 4 {
                                let last = bytes.last_mut().unwrap();
                                *last ^= 1;
                            }
                            let planned = if mode == 5 {
                                FrameBudget::new(160, 1).unwrap()
                            } else {
                                budget()
                            };
                            let result = session.commit_fixture(
                                &Provider,
                                &clock,
                                deadline(),
                                &bytes,
                                planned,
                            );
                            if mode < 2 {
                                assert!(result.is_ok());
                            } else if mode < 4 {
                                assert!(matches!(
                                    result,
                                    Err(CommitError::Stopped(JournalError::Policy(
                                        PolicyError::WriterStopped
                                    )))
                                ));
                            } else {
                                assert!(matches!(
                                    result,
                                    Err(CommitError::Stopped(JournalError::Append(_)))
                                ));
                            }
                            assert_eq!(old.identity(), initial);
                            drop(old);
                            if mode == 3 {
                                assert!(matches!(
                                    session.capture(),
                                    Err(JournalError::Policy(PolicyError::WriterStopped))
                                ));
                                assert_eq!(
                                    session.published.lock().err().unwrap().into_inner().readers,
                                    0
                                );
                            } else {
                                let new = session.capture().unwrap();
                                assert_eq!(
                                    new.identity().committed_sequence,
                                    Sequence::from_u64(if mode < 2 { 3 } else { 2 })
                                );
                            }
                            if mode >= 2 {
                                assert!(matches!(
                                    session.commit_fixture(
                                        &Provider,
                                        &clock,
                                        deadline(),
                                        &frame(3),
                                        budget()
                                    ),
                                    Err(CommitError::Stopped(JournalError::Policy(
                                        PolicyError::WriterStopped
                                    )))
                                ));
                            }
                        },
                    )
                    .unwrap();
            });
        }
    }
}
