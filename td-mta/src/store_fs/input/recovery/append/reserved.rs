//! Bind one physical append to its exclusively borrowed reservation ledger.
use super::{
    AppendError, AppendStep, JournalAppend, Operations, Real, ScannedJournal, SyncedAppend,
};
use crate::{
    admission::{
        quota::Kind,
        writer::{self, FrameBudget, FrameId, GuardedAppend, WriterLedger},
    },
    format::JOURNAL_HEADER_BYTES,
    ports::{Crypto, Tick},
};

#[derive(Debug)]
pub enum ReservedAppendError {
    Append(AppendError),
    Ledger(writer::Error),
}
impl std::fmt::Display for ReservedAppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Append(e) => e.fmt(f),
            Self::Ledger(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for ReservedAppendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Append(e) => Some(e),
            Self::Ledger(e) => Some(e),
        }
    }
}
impl From<AppendError> for ReservedAppendError {
    fn from(e: AppendError) -> Self {
        Self::Append(e)
    }
}
impl From<writer::Error> for ReservedAppendError {
    fn from(e: writer::Error) -> Self {
        Self::Ledger(e)
    }
}

/// Finish reconciles exact durable charges. Any error or unfinished drop stops admission.
#[must_use = "finish the reserved append or leave its ledger stopped for recovery"]
pub struct ReservedAppend<'r, 'b, 'l, 'a> {
    append: JournalAppend<'r, 'b>,
    guard: GuardedAppend<'l, 'a>,
}
/// Durable append whose exact charges were reconciled by its bound ledger.
/// Reader visibility and final transaction policy are still external.
pub struct ReconciledAppend<'r> {
    durable: SyncedAppend<'r>,
}
impl ReconciledAppend<'_> {
    pub fn durable(&self) -> &SyncedAppend<'_> {
        &self.durable
    }
}
impl<'r> ScannedJournal<'r> {
    /// Caller supplies the ledger recovered for this selected account and holds
    /// stopped-store exclusion through completion, except for JournalSession
    /// queries confined to its retained committed prefixes.
    /// Constructor refusal writes nothing and leaves
    /// all preexisting reservation state unchanged. No final graph policy
    /// or protocol acknowledgment is established.
    ///
    /// ```compile_fail,E0499
    /// use td_mta::{admission::writer::{Grant, WriterLedger}, ports::Tick, store_fs::ScannedJournal};
    /// fn overlap(scan: ScannedJournal<'_>, bytes: &[u8], ledger: &mut WriterLedger<'_>, grant: Grant) {
    ///     let mut append = scan.append_reserved(&td_crypto::Provider, bytes, ledger, grant.frame().unwrap(), Tick(2)).unwrap();
    ///     ledger.cancel(grant).unwrap();
    ///     append.advance().unwrap();
    /// }
    /// ```
    pub fn append_reserved<'b, 'l, 'a>(
        self,
        crypto: &impl Crypto,
        bytes: &'b [u8],
        ledger: &'l mut WriterLedger<'a>,
        frame: FrameId,
        now: Tick,
    ) -> Result<ReservedAppend<'r, 'b, 'l, 'a>, ReservedAppendError> {
        bind(self.append_frame(crypto, bytes)?, ledger, frame, now)
    }
}
impl<'r> ReconciledAppend<'r> {
    /// Consume the retained durable boundary to append its successor without a
    /// full journal rescan. Keep the same account and append exclusion contract and
    /// ledger. Constructor refusal writes nothing; this consumed owner is then
    /// closed, so rescan before a later attempt. Publication remains external.
    pub fn append_reserved<'b, 'l, 'a>(
        self,
        crypto: &impl Crypto,
        bytes: &'b [u8],
        ledger: &'l mut WriterLedger<'a>,
        frame: FrameId,
        now: Tick,
    ) -> Result<ReservedAppend<'r, 'b, 'l, 'a>, ReservedAppendError> {
        bind(
            self.durable.append_frame(crypto, bytes)?,
            ledger,
            frame,
            now,
        )
    }
}
fn bind<'r, 'b, 'l, 'a>(
    append: JournalAppend<'r, 'b>,
    ledger: &'l mut WriterLedger<'a>,
    frame: FrameId,
    now: Tick,
) -> Result<ReservedAppend<'r, 'b, 'l, 'a>, ReservedAppendError> {
    let invalid = || ReservedAppendError::Ledger(writer::Error::Invalid);
    let actual = FrameBudget::new(
        u64::try_from(append.bytes.len()).map_err(|_| invalid())?,
        u64::try_from(append.operations).map_err(|_| invalid())?,
    )?;
    let prior_bytes = append
        .file
        .len()
        .checked_sub(JOURNAL_HEADER_BYTES as u64)
        .ok_or_else(invalid)?;
    let prior_operations = append
        .total_operations
        .checked_sub(append.operations)
        .ok_or_else(invalid)?;
    if ledger.used(Kind::ActiveJournalBytes)? != prior_bytes
        || ledger.used(Kind::ActiveJournalOperations)?
            != u64::try_from(prior_operations).map_err(|_| invalid())?
    {
        return Err(invalid());
    }
    let guard = ledger.guard_append(frame, actual, now)?;
    Ok(ReservedAppend { append, guard })
}
impl<'r> ReservedAppend<'r, '_, '_, '_> {
    pub fn written_bytes(&self) -> usize {
        self.append.written_bytes()
    }
    pub fn is_failed(&self) -> bool {
        self.append.is_failed()
    }
    pub fn advance(&mut self) -> Result<AppendStep, ReservedAppendError> {
        self.advance_using(&mut Real)
    }
    fn advance_using(
        &mut self,
        ops: &mut impl Operations,
    ) -> Result<AppendStep, ReservedAppendError> {
        match self.append.advance_using(ops) {
            Ok(step) => Ok(step),
            Err(e) => {
                self.guard.stop();
                Err(e.into())
            }
        }
    }
    /// Return evidence only after sync, confirmation and exact ledger reconciliation.
    /// Caller must still publish committed visibility before acknowledging a client.
    pub fn finish(self) -> Result<ReconciledAppend<'r>, ReservedAppendError> {
        let Self { append, guard } = self;
        let synced = append.finish()?;
        guard.synced()?;
        Ok(ReconciledAppend { durable: synced })
    }
}

#[cfg(test)]
pub use tests::probe as probe_reserved_append;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::super::tests::{many, next, scan, setup, Faults};
    use super::*;
    use crate::{
        admission::{
            logical::{Cell, Error as LogicalError},
            quota::{Charge, Usage},
            writer::{Grant, Phase},
            DiskLimits, Plan, ViewMode, WorkLimits,
        },
        limits::Limits,
        ownership::{self, SlotState},
        ports::Deadline,
    };
    use td_crypto::Provider;
    fn with_ledger(bytes: u64, operations: u64, run: impl FnOnce(&mut WriterLedger<'_>)) {
        let plan: Plan = DiskLimits::default()
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
        let mut ledger = WriterLedger::new(&plan, 1232, used, &mut states, &mut cells).unwrap();
        run(&mut ledger);
    }
    fn grant(ledger: &mut WriterLedger<'_>, bytes: u64, operations: u64) -> Grant {
        let requests = [[
            Charge {
                kind: Kind::BodyBytes,
                amount: 100,
            },
            Charge::ZERO,
            Charge::ZERO,
            Charge::ZERO,
        ]];
        for _ in 0..1000 {
            let prep = ledger
                .prepare(
                    &requests,
                    Some(FrameBudget::new(bytes, operations).unwrap()),
                    Deadline::after(Tick(0), 100).unwrap(),
                    Tick(0),
                )
                .unwrap();
            match prep.install(Tick(1)) {
                Err(writer::Error::Logical(LogicalError::Slot(ownership::Error::Contended))) => {
                    std::thread::yield_now()
                }
                result => return result.unwrap(),
            }
        }
        panic!("reservation contention did not converge");
    }
    fn complete<'r>(mut append: ReservedAppend<'r, '_, '_, '_>) -> ReconciledAppend<'r> {
        for _ in 0..70 {
            if append.advance().unwrap() == AppendStep::Complete {
                return append.finish().unwrap();
            }
        }
        panic!("bounded append did not complete");
    }
    #[test]
    fn reconciled_boundaries_chain_without_rescanning_and_charge_each_successor() {
        with_ledger(160, 2, |ledger| {
            let (_dir, root, selection) = setup();
            let bytes = next(3);
            let first = grant(ledger, 1000, 4);
            let mut receipt = complete(
                scan(&root, &selection)
                    .append_reserved(&Provider, &bytes, ledger, first.frame().unwrap(), Tick(2))
                    .unwrap(),
            );
            ledger.cancel(first).unwrap();
            for sequence in 4..=6 {
                let bytes = next(sequence);
                let reserved = grant(ledger, 1000, 4);
                receipt = complete(
                    receipt
                        .append_reserved(
                            &Provider,
                            &bytes,
                            ledger,
                            reserved.frame().unwrap(),
                            Tick(2),
                        )
                        .unwrap(),
                );
                assert_eq!(receipt.durable().through().number(), sequence);
                assert_eq!(receipt.durable().end(), 96 + 160 * (sequence - 1));
                assert_eq!(
                    receipt.durable().total_operations(),
                    2 * (sequence as usize - 1)
                );
                assert_eq!(
                    ledger.used(Kind::ActiveJournalBytes).unwrap(),
                    160 * (sequence - 1)
                );
                assert_eq!(
                    ledger.used(Kind::ActiveJournalOperations).unwrap(),
                    2 * (sequence - 1)
                );
                assert_eq!(ledger.pending(Kind::ActiveJournalBytes).unwrap(), 0);
                ledger.cancel(reserved).unwrap();
            }
            let replayed = scan(&root, &selection);
            assert_eq!(replayed.summary().through(), receipt.durable().through());
            assert_eq!(replayed.file().len(), receipt.durable().end());
            assert!(!replayed.has_incomplete_tail());
        });
    }
    #[test]
    fn chained_constructor_refuses_bad_frames_changed_current_extent_and_inode_before_writes() {
        use crate::store_paths::{AccountEntry, Name, Number};
        for mode in 0..5 {
            with_ledger(160, 2, |ledger| {
                let (dir, root, selection) = setup();
                let bytes = next(3);
                let first = grant(ledger, 1000, 4);
                let receipt = complete(
                    scan(&root, &selection)
                        .append_reserved(&Provider, &bytes, ledger, first.frame().unwrap(), Tick(2))
                        .unwrap(),
                );
                ledger.cancel(first).unwrap();
                let account = receipt.durable().current().account;
                let name =
                    Name::account(account, AccountEntry::Journal(Number::new(2).unwrap())).unwrap();
                let path = dir.path.join(name.as_path().unwrap());
                let mut bytes = next(if mode == 0 { 5 } else { 4 });
                match mode {
                    1 => *bytes.last_mut().unwrap() ^= 1,
                    2 => {
                        let name = Name::account(account, AccountEntry::Current).unwrap();
                        std::fs::write(
                            dir.path.join(name.as_path().unwrap()),
                            [0; crate::format::CURRENT_BYTES],
                        )
                        .unwrap();
                    }
                    3 => {
                        use std::io::Write;
                        std::fs::OpenOptions::new()
                            .append(true)
                            .open(&path)
                            .unwrap()
                            .write_all(b"late")
                            .unwrap();
                    }
                    4 => {
                        let prior = std::fs::read(&path).unwrap();
                        std::fs::remove_file(&path).unwrap();
                        use std::io::Write;
                        use std::os::unix::fs::OpenOptionsExt;
                        std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .mode(0o600)
                            .open(&path)
                            .unwrap()
                            .write_all(&prior)
                            .unwrap();
                    }
                    _ => (),
                }
                let prior = std::fs::read(&path).unwrap();
                let reserved = grant(ledger, 1000, 4);
                assert!(matches!(
                    receipt.append_reserved(
                        &Provider,
                        &bytes,
                        ledger,
                        reserved.frame().unwrap(),
                        Tick(2)
                    ),
                    Err(ReservedAppendError::Append(AppendError::Rejected(_)))
                ));
                assert_eq!(std::fs::read(&path).unwrap(), prior);
                assert_eq!(ledger.phase(), Phase::Open);
                assert_eq!(ledger.used(Kind::ActiveJournalBytes).unwrap(), 320);
                ledger.cancel(reserved).unwrap();
            });
        }
    }
    #[test]
    fn chained_write_failure_keeps_prior_durable_prefix_and_full_pending_charge() {
        with_ledger(160, 2, |ledger| {
            let (_dir, root, selection) = setup();
            let bytes = next(3);
            let first = grant(ledger, 1000, 4);
            let receipt = complete(
                scan(&root, &selection)
                    .append_reserved(&Provider, &bytes, ledger, first.frame().unwrap(), Tick(2))
                    .unwrap(),
            );
            ledger.cancel(first).unwrap();
            let bytes = next(4);
            let reserved = grant(ledger, 1000, 4);
            let mut append = receipt
                .append_reserved(
                    &Provider,
                    &bytes,
                    ledger,
                    reserved.frame().unwrap(),
                    Tick(2),
                )
                .unwrap();
            let mut ops = Faults::new(1);
            assert_eq!(
                append.advance_using(&mut ops).unwrap(),
                AppendStep::Written { bytes: 7 }
            );
            assert!(
                matches!(append.advance_using(&mut ops), Err(ReservedAppendError::Append(AppendError::Indeterminate(ref e))) if e.kind() == std::io::ErrorKind::StorageFull)
            );
            assert!(append.finish().is_err());
            assert_eq!(ledger.phase(), Phase::Stopped);
            assert_eq!(ledger.used(Kind::ActiveJournalBytes).unwrap(), 320);
            assert_eq!(ledger.used(Kind::ActiveJournalOperations).unwrap(), 4);
            assert_eq!(ledger.pending(Kind::ActiveJournalBytes).unwrap(), 1000);
            assert_eq!(ledger.pending(Kind::ActiveJournalOperations).unwrap(), 4);
            assert!(matches!(
                ledger.cancel(reserved),
                Err(writer::Error::Logical(LogicalError::Busy))
            ));
            let replayed = scan(&root, &selection);
            assert_eq!(replayed.summary().through().number(), 3);
            assert_eq!(replayed.valid_bytes(), 416);
            assert_eq!(replayed.file().len(), 423);
            assert!(replayed.has_incomplete_tail());
        });
    }
    #[test]
    fn durable_finish_charges_actual_frame_and_preserves_other_reservations() {
        with_ledger(160, 2, |ledger| {
            let (_dir, root, selection) = setup();
            let frame = next(3);
            let grant = grant(ledger, 1000, 4);
            let mut append = scan(&root, &selection)
                .append_reserved(&Provider, &frame, ledger, grant.frame().unwrap(), Tick(2))
                .unwrap();
            assert!(std::mem::size_of_val(&append) <= 2048);
            assert_eq!(append.written_bytes(), 0);
            assert_eq!(
                append.advance().unwrap(),
                AppendStep::Written { bytes: 160 }
            );
            assert_eq!(append.advance().unwrap(), AppendStep::Synced);
            assert_eq!(append.advance().unwrap(), AppendStep::Complete);
            assert_eq!(append.advance().unwrap(), AppendStep::Complete);
            let proof = append.finish().unwrap();
            assert_eq!(proof.durable().end(), 416);
            assert!(std::mem::size_of_val(&proof) <= 1024);
            assert_eq!(ledger.phase(), Phase::Open);
            assert_eq!(ledger.used(Kind::ActiveJournalBytes).unwrap(), 320);
            assert_eq!(ledger.used(Kind::ActiveJournalOperations).unwrap(), 4);
            assert_eq!(ledger.pending(Kind::ActiveJournalBytes).unwrap(), 0);
            assert_eq!(ledger.pending(Kind::ActiveJournalOperations).unwrap(), 0);
            assert_eq!(ledger.pending(Kind::BodyBytes).unwrap(), 100);
            ledger.cancel(grant).unwrap();
            assert_eq!(ledger.pending(Kind::BodyBytes).unwrap(), 0);
        });
    }
    #[test]
    fn all_append_faults_stop_admission_and_preserve_full_busy_charges() {
        for mode in 1..10 {
            with_ledger(160, 2, |ledger| {
                let (_dir, root, selection) = setup();
                let frame = next(3);
                let grant = grant(ledger, 1000, 4);
                let mut append = scan(&root, &selection)
                    .append_reserved(&Provider, &frame, ledger, grant.frame().unwrap(), Tick(2))
                    .unwrap();
                let mut ops = Faults::new(mode);
                for _ in 0..70 {
                    if append.advance_using(&mut ops).is_err() {
                        break;
                    }
                }
                assert!(append.is_failed());
                assert!(matches!(
                    append.advance_using(&mut ops),
                    Err(ReservedAppendError::Append(AppendError::Failed))
                ));
                assert!(append.finish().is_err());
                stopped(ledger, grant);
            });
        }
    }
    #[test]
    fn step_failure_stops_admission_without_relying_on_guard_drop() {
        with_ledger(160, 2, |ledger| {
            let (_dir, root, selection) = setup();
            let frame = next(3);
            let grant = grant(ledger, 1000, 4);
            {
                let mut append = scan(&root, &selection)
                    .append_reserved(&Provider, &frame, ledger, grant.frame().unwrap(), Tick(2))
                    .unwrap();
                let mut ops = Faults::new(1);
                assert_eq!(
                    append.advance_using(&mut ops).unwrap(),
                    AppendStep::Written { bytes: 7 }
                );
                assert!(
                    matches!(append.advance_using(&mut ops), Err(ReservedAppendError::Append(AppendError::Indeterminate(ref e))) if e.kind() == std::io::ErrorKind::StorageFull)
                );
                let ReservedAppend { append, guard } = append;
                // Bypass only the scalar guard destructor; close the physical file normally.
                std::mem::forget(guard);
                drop(append);
            }
            stopped(ledger, grant);
        });
    }
    fn stopped(ledger: &mut WriterLedger<'_>, grant: Grant) {
        assert_eq!(ledger.phase(), Phase::Stopped);
        assert_eq!(ledger.used(Kind::ActiveJournalBytes).unwrap(), 160);
        assert_eq!(ledger.used(Kind::ActiveJournalOperations).unwrap(), 2);
        assert_eq!(ledger.pending(Kind::ActiveJournalBytes).unwrap(), 1000);
        assert_eq!(ledger.pending(Kind::ActiveJournalOperations).unwrap(), 4);
        assert_eq!(ledger.pending(Kind::BodyBytes).unwrap(), 100);
        assert!(matches!(
            ledger.cancel(grant),
            Err(writer::Error::Logical(LogicalError::Busy))
        ));
        assert!(matches!(
            ledger.begin_checkpoint(),
            Err(writer::Error::Stopped)
        ));
    }
    #[test]
    fn drop_at_every_phase_and_premature_finish_keep_charges_until_recovery() {
        for steps in 0..4 {
            for finish in [false, true] {
                if steps == 3 && finish {
                    continue;
                }
                with_ledger(160, 2, |ledger| {
                    let (_dir, root, selection) = setup();
                    let frame = next(3);
                    let grant = grant(ledger, 1000, 4);
                    let mut append = scan(&root, &selection)
                        .append_reserved(&Provider, &frame, ledger, grant.frame().unwrap(), Tick(2))
                        .unwrap();
                    for _ in 0..steps {
                        append.advance().unwrap();
                    }
                    if finish {
                        assert!(matches!(
                            append.finish(),
                            Err(ReservedAppendError::Append(AppendError::Incomplete))
                        ));
                    } else {
                        drop(append);
                    }
                    stopped(ledger, grant);
                });
            }
        }
    }
    #[test]
    fn constructor_refusal_writes_nothing_and_keeps_grant_cancelable() {
        for mode in 0..6 {
            with_ledger(
                if mode == 4 { 161 } else { 160 },
                if mode == 5 { 3 } else { 2 },
                |ledger| {
                    let (_dir, root, selection) = setup();
                    let frame = if mode == 1 {
                        many(3, 2, true)
                    } else {
                        next(if mode == 0 { 4 } else { 3 })
                    };
                    let grant = grant(
                        ledger,
                        if mode == 1 { 160 } else { 1000 },
                        if mode == 2 { 1 } else { 2 },
                    );
                    let result = scan(&root, &selection).append_reserved(
                        &Provider,
                        &frame,
                        ledger,
                        grant.frame().unwrap(),
                        Tick(if mode == 3 { 100 } else { 2 }),
                    );
                    assert!(
                        matches!(
                            (mode, &result),
                            (
                                0,
                                Err(ReservedAppendError::Append(AppendError::Rejected(_)))
                            ) | (
                                1 | 2,
                                Err(ReservedAppendError::Ledger(writer::Error::Logical(
                                    LogicalError::Invalid
                                )))
                            ) | (
                                3,
                                Err(ReservedAppendError::Ledger(writer::Error::Logical(
                                    LogicalError::Expired
                                )))
                            ) | (
                                4 | 5,
                                Err(ReservedAppendError::Ledger(writer::Error::Invalid))
                            )
                        ),
                        "wrong refusal for mode {mode}"
                    );
                    drop(result);
                    assert_eq!(ledger.phase(), Phase::Open);
                    ledger.cancel(grant).unwrap();
                    assert_eq!(ledger.pending(Kind::ActiveJournalBytes).unwrap(), 0);
                    let unchanged = scan(&root, &selection);
                    assert_eq!(unchanged.summary().through().number(), 2);
                    assert_eq!(unchanged.file().len(), 256);
                    assert!(!unchanged.has_incomplete_tail());
                },
            );
        }
    }
    pub fn probe(mut snapshot: impl FnMut()) {
        use super::super::super::super::super::tests::Fixture;
        use super::super::tests::setup_with;
        for maximum in [false, true] {
            use std::io::ErrorKind;
            for (fault, abandon_steps, expected_error) in [
                (None, None, None),    // ordinary production path
                (Some(0), None, None), // successful short writes
                (Some(1), None, Some(ErrorKind::StorageFull)),
                (Some(4), None, Some(ErrorKind::Other)),
                (Some(6), None, Some(ErrorKind::InvalidData)),
                (None, Some(0), None), // abandon before writing
                (None, Some(1), None), // abandon after writing
                (None, Some(3), None), // abandon after confirmation
            ] {
                let fixture = if maximum {
                    Fixture::maximum_root()
                } else {
                    Fixture::new()
                };
                let (_dir, root, selection) = setup_with(fixture);
                let frame = next(3);
                let scanned = scan(&root, &selection);
                with_ledger(160, 2, |ledger| {
                    let mut ops = fault.map(Faults::new);
                    snapshot();
                    let grant = grant(ledger, 1000, 4);
                    let mut append = scanned
                        .append_reserved(&Provider, &frame, ledger, grant.frame().unwrap(), Tick(2))
                        .unwrap();
                    if let Some(steps) = abandon_steps {
                        for _ in 0..steps {
                            append.advance().unwrap();
                        }
                        drop(append);
                        stopped(ledger, grant);
                    } else {
                        let mut complete = false;
                        for _ in 0..70 {
                            let step = match ops.as_mut() {
                                Some(ops) => append.advance_using(ops),
                                None => append.advance(),
                            };
                            match step {
                                Ok(AppendStep::Complete) => {
                                    complete = true;
                                    break;
                                }
                                Ok(_) => (),
                                Err(ReservedAppendError::Append(AppendError::Indeterminate(e))) => {
                                    assert_eq!(Some(e.kind()), expected_error);
                                    break;
                                }
                                Err(e) => panic!("unexpected reserved append error: {e}"),
                            }
                        }
                        assert_eq!(complete, expected_error.is_none());
                        if complete {
                            {
                                let evidence = append.finish().unwrap();
                                assert_eq!(evidence.durable().end(), 416);
                            }
                            assert_eq!(ledger.used(Kind::ActiveJournalBytes).unwrap(), 320);
                            assert_eq!(ledger.used(Kind::ActiveJournalOperations).unwrap(), 4);
                            ledger.cancel(grant).unwrap();
                            assert_eq!(ledger.pending(Kind::BodyBytes).unwrap(), 0);
                        } else {
                            assert!(matches!(
                                append.finish(),
                                Err(ReservedAppendError::Append(AppendError::Failed))
                            ));
                            stopped(ledger, grant);
                        }
                    }
                    snapshot();
                });
            }
        }
    }
}
