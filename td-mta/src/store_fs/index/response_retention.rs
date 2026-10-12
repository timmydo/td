//! One coordinator reservation owns its private file through final unlink.
use super::*;
use crate::{
    admission::{
        logical::EffectTicket,
        work::{Charge as WorkCharge, Meter},
    },
    store_fs::{request_spool::Temporary, RequestSpool},
};

#[derive(Debug)]
pub enum ResponseError {
    Store(ports::Error),
    Ledger(logical::Error),
}
impl From<ports::Error> for ResponseError {
    fn from(error: ports::Error) -> Self {
        Self::Store(error)
    }
}
impl From<logical::Error> for ResponseError {
    fn from(error: logical::Error) -> Self {
        Self::Ledger(error)
    }
}
impl std::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => error.fmt(f),
            Self::Ledger(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for ResponseError {}

/// Raw bounded retention; publication and JSON validity belong to the dispatcher.
/// No path, file descriptor, mutable ledger or cleanup proof escapes this owner.
#[must_use = "retained response storage remains charged until discarded"]
pub struct ResponseRetention<'c, 's, 'r, 'sr, 'a, P> {
    coordinator: &'c StoreCoordinator<'r, 'a, P>,
    temporary: Option<Temporary<'s, 'sr>>,
    lease: Option<LeaseId>,
    effect: Option<EffectTicket>,
    maximum: u64,
    length: u64,
    published: u64,
    deadline: Deadline,
    last: Tick,
    meter: Meter,
    failed: Option<ports::Error>,
}
/// Sort files use the same custody failures as response retention.
pub type SortError = ResponseError;

/// One bounded scratch file charged to SortBytes until proven removal.
/// Runs and merge metadata are the sorter caller's responsibility.
#[must_use = "sort scratch remains charged until discarded"]
pub struct SortRetention<'c, 's, 'r, 'sr, 'a, P> {
    inner: ResponseRetention<'c, 's, 'r, 'sr, 'a, P>,
}
impl<'r, 'a, P> StoreCoordinator<'r, 'a, P> {
    /// Trusted worker entry after authentication, before any method effects.
    /// Reserve the entire configured per-request cap in the existing ledger.
    /// Pass the original outer exchange deadline; the dispatcher separately
    /// enforces tighter execution and transmit phase budgets.
    pub fn retain_response<'c, 's, 'sr>(
        &'c self,
        spool: &'s RequestSpool<'sr>,
        deadline: Deadline,
    ) -> Result<ResponseRetention<'c, 's, 'r, 'sr, 'a, P>, ResponseError> {
        self.retain_temporary(spool, deadline, Kind::ResponseBytes, self.response_bytes)
    }
    /// Reserve one sort file's whole cap before creating it. All simultaneous
    /// inputs, outputs and other sort scratch share the existing SortBytes quota.
    pub fn retain_sort<'c, 's, 'sr>(
        &'c self,
        spool: &'s RequestSpool<'sr>,
        maximum: u64,
        deadline: Deadline,
    ) -> Result<SortRetention<'c, 's, 'r, 'sr, 'a, P>, SortError> {
        if maximum == 0 {
            return Err(ports::Error::Invalid.into());
        }
        if maximum > self.sort_bytes {
            return Err(ports::Error::Capacity.into());
        }
        self.retain_temporary(spool, deadline, Kind::SortBytes, maximum)
            .map(|inner| SortRetention { inner })
    }
    fn retain_temporary<'c, 's, 'sr>(
        &'c self,
        spool: &'s RequestSpool<'sr>,
        deadline: Deadline,
        kind: Kind,
        maximum: u64,
    ) -> Result<ResponseRetention<'c, 's, 'r, 'sr, 'a, P>, ResponseError> {
        let mut state = self.try_state()?;
        if state.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        // Preserve one cell for simultaneous durable publication/maintenance.
        if state.ledger.available_cells() < 2 {
            return Err(logical::Error::Full.into());
        }
        let mut last = self.store.clock.sample()?.monotonic;
        let now = check_time(&self.store, deadline, &mut last)?.monotonic;
        let lease = state.ledger.reserve(
            &[[
                Charge {
                    kind,
                    amount: maximum,
                },
                Charge::ZERO,
                Charge::ZERO,
                Charge::ZERO,
            ]],
            deadline,
            now,
        )?;
        let effect = match state
            .ledger
            .part(lease, 0)
            .and_then(|part| state.ledger.begin_effect(part, [maximum, 0, 0, 0], now))
        {
            Ok(effect) => effect,
            Err(error) => {
                if state.ledger.cancel(lease).is_err() {
                    state.stopped = true;
                    state.recoverable_files = false;
                }
                return Err(error.into());
            }
        };
        drop(state);
        let mut response = ResponseRetention {
            coordinator: self,
            temporary: None,
            lease: Some(lease),
            effect: Some(effect),
            maximum,
            length: 0,
            published: 0,
            deadline,
            last,
            meter: Meter::new(
                deadline,
                WorkCharge {
                    io_bytes: self.work.foreground_io_bytes,
                    records: self.work.foreground_records,
                    output_bytes: self.work.foreground_io_bytes,
                    unlinks: 0,
                },
            ),
            failed: None,
        };
        response.temporary = Some(if kind == Kind::SortBytes {
            spool.reserve_sort()?
        } else {
            spool.reserve()?
        });
        response.temporary()?.create()?;
        response.check()?;
        Ok(response)
    }
}
impl<'c, 's, 'r, 'sr, 'a, P> ResponseRetention<'c, 's, 'r, 'sr, 'a, P> {
    pub(crate) fn method_sort_context(
        &mut self,
        phase: Deadline,
    ) -> Result<(&'c StoreCoordinator<'r, 'a, P>, &'s RequestSpool<'sr>, u64), ports::Error> {
        if !self.admit_before(phase)? {
            return Err(ports::Error::Deadline);
        }
        let cap = self.coordinator.sort_bytes / 2;
        if cap == 0 {
            return Err(ports::Error::Capacity);
        }
        Ok((
            self.coordinator,
            self.temporary
                .as_ref()
                .ok_or(ports::Error::Invalid)?
                .spool(),
            cap,
        ))
    }
    pub(crate) fn method_view(
        &mut self,
        account: AccountId,
        phase: Deadline,
    ) -> Result<IndexReadView<'c, 'r>, ports::Error> {
        if !self.admit_before(phase)? {
            return Err(ports::Error::Deadline);
        }
        self.coordinator.store.view(account, phase)
    }

    pub fn len(&self) -> u64 {
        self.length
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub fn maximum(&self) -> u64 {
        self.maximum
    }
    pub fn published_len(&self) -> u64 {
        self.published
    }
    /// A prewrite cap refusal leaves None; terminal I/O/clock/work errors remain
    /// here and cannot be repaired by rolling back a tail.
    pub const fn failure(&self) -> Option<ports::Error> {
        self.failed
    }
    pub fn work_failure(&self) -> Option<crate::admission::work::Stop> {
        self.meter.stopped()
    }
    fn temporary(&mut self) -> Result<&mut Temporary<'s, 'sr>, ports::Error> {
        self.temporary.as_mut().ok_or(ports::Error::Invalid)
    }
    fn check(&mut self) -> Result<(), ports::Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = check_time(&self.coordinator.store, self.deadline, &mut self.last).map(|_| ());
        self.record(result)
    }
    fn record<T>(&mut self, result: Result<T, ports::Error>) -> Result<T, ports::Error> {
        result.inspect_err(|error| {
            self.failed.get_or_insert(*error);
        })
    }
    fn charge(&mut self, bytes: usize, output: bool) -> Result<(), ports::Error> {
        let result = self
            .meter
            .charge(
                self.last,
                WorkCharge {
                    io_bytes: bytes as u64,
                    records: 1,
                    output_bytes: if output { bytes as u64 } else { 0 },
                    unlinks: 0,
                },
            )
            .map_err(|_| ports::Error::Capacity);
        self.record(result)
    }
    /// Admit one bounded encoder/walker turn, including turns needing no I/O.
    /// Charge the original foreground record budget and observe the original
    /// outer deadline; callers additionally enforce execution-phase limits.
    pub fn admit_work_turn(&mut self) -> Result<(), ports::Error> {
        self.check()?;
        self.charge(0, false)?;
        self.check()
    }
    /// Charge a CPU turn and report whether the narrower execution phase is
    /// still open. Phase expiry does not retire the outer owner: it must retain
    /// capacity and time to encode method errors and transmit the response.
    pub fn admit_before(&mut self, phase: ports::Deadline) -> Result<bool, ports::Error> {
        if phase >= self.deadline {
            return Err(ports::Error::Invalid);
        }
        self.admit_work_turn()?;
        Ok(!phase.expired(self.last))
    }
    #[cfg(test)]
    pub(crate) fn test_record_budget(&mut self, records: u64) {
        self.meter = Meter::new(
            self.deadline,
            WorkCharge {
                records,
                io_bytes: u64::MAX,
                output_bytes: u64::MAX,
                unlinks: 0,
            },
        );
    }
    /// One worker turn, at most 64 KiB. Cap refusal writes nothing and permits
    /// abandoning the unpublished tail; I/O/work/clock failure is terminal.
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), ports::Error> {
        self.check()?;
        let next = self
            .length
            .checked_add(bytes.len() as u64)
            .ok_or(ports::Error::Capacity)?;
        if bytes.len() > crate::limits::SQLITE_BODY_CHUNK_BYTES || next > self.maximum {
            return Err(ports::Error::Capacity);
        }
        self.charge(bytes.len(), true)?;
        let offset = self.length;
        let result = self.temporary()?.write_at(offset, bytes);
        self.record(result)?;
        self.length = next;
        self.check()
    }
    /// Seal an immutable prefix only after the dispatcher completes valid JSON.
    pub fn publish(&mut self) -> Result<u64, ports::Error> {
        self.check()?;
        self.published = self.length;
        Ok(self.published)
    }
    /// A failed read-only method may discard only its unpublished tail.
    pub fn rollback_tail(&mut self) -> Result<(), ports::Error> {
        self.check()?;
        self.charge(0, false)?;
        let length = self.published;
        let result = self.temporary()?.truncate(length);
        self.record(result)?;
        self.length = length;
        self.check()
    }
    /// Exact bounded reads of already published bytes, never provisional output.
    pub fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<(), ports::Error> {
        self.check()?;
        let end = offset
            .checked_add(output.len() as u64)
            .ok_or(ports::Error::Capacity)?;
        if output.len() > crate::limits::SQLITE_BODY_CHUNK_BYTES {
            return Err(ports::Error::Capacity);
        }
        if end > self.published {
            return Err(ports::Error::Invalid);
        }
        self.charge(output.len(), false)?;
        let result = self.temporary()?.read_at(offset, output);
        self.record(result)?;
        self.check()
    }
    pub fn discard(mut self) -> Result<(), ResponseError> {
        self.cleanup()
    }
    fn cleanup(&mut self) -> Result<(), ResponseError> {
        if self.effect.is_none() {
            return Ok(());
        }
        // Unlink and close before releasing any reservation; not subject
        // to an expired request deadline. Drop runs only on storage workers.
        let cleanup = self.temporary.as_mut().map_or(Ok(()), Temporary::cleanup);
        let mut state = lock(&self.coordinator.state)?;
        let effect = self.effect.as_mut().ok_or(ports::Error::Invalid)?;
        let result = state.ledger.complete_effect(
            effect,
            if cleanup.is_ok() {
                EffectResult::Proven([0; 4])
            } else {
                EffectResult::Uncertain
            },
        );
        if result.is_err() {
            state.stopped = true;
            state.recoverable_files = false;
            return result.map_err(Into::into);
        }
        self.effect = None;
        if let Some(lease) = self.lease.take() {
            if let Err(error) = state.ledger.cancel(lease) {
                state.stopped = true;
                state.recoverable_files = false;
                return Err(error.into());
            }
        }
        cleanup.map_err(Into::into)
    }
}
impl<P> SortRetention<'_, '_, '_, '_, '_, P> {
    pub fn len(&self) -> u64 {
        self.inner.len()
    }
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
    pub fn maximum(&self) -> u64 {
        self.inner.maximum()
    }
    pub fn failure(&self) -> Option<ports::Error> {
        self.inner.failure()
    }
    pub fn work_failure(&self) -> Option<crate::admission::work::Stop> {
        self.inner.work_failure()
    }
    /// Passive readable-prefix metadata; no freshness or view check is implied.
    pub fn sealed_len(&self) -> u64 {
        self.inner.published_len()
    }
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), ports::Error> {
        self.inner.append(bytes)
    }
    /// Make the current prefix readable; later appends remain unreadable until
    /// another seal. This does not publish any client-visible query result.
    pub fn seal(&mut self) -> Result<u64, ports::Error> {
        self.inner.admit_work_turn()?;
        // Shared publication remains repeatable and permits later appends.
        self.inner.publish()
    }
    pub fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<(), ports::Error> {
        self.inner.read_at(offset, output)
    }
    /// Discard this file's old runs only after callers stop using their offsets.
    /// The original quota, clock and work meter remain held across reuse.
    pub fn reset(&mut self) -> Result<(), ports::Error> {
        self.inner.check()?;
        self.inner.charge(0, false)?;
        let result = self
            .inner
            .temporary()
            .and_then(|temporary| temporary.truncate(0));
        self.inner.record(result)?;
        self.inner.length = 0;
        self.inner.published = 0;
        self.inner.check()
    }
    pub fn discard(self) -> Result<(), SortError> {
        self.inner.discard()
    }
}
impl<P> Drop for ResponseRetention<'_, '_, '_, '_, '_, P> {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        admission::{DiskLimits, ViewMode, WorkLimits},
        ids::StoreEpoch,
        limits::Limits,
        store_fs::tests::Fixture,
    };
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        sync::atomic::{AtomicU64, Ordering},
    };
    struct Timer(AtomicU64);
    impl Clock for Timer {
        fn sample(&self) -> Result<Time, ports::Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(self.0.load(Ordering::SeqCst)),
            })
        }
    }
    fn deadline() -> Deadline {
        Deadline::after(Tick(0), 100).unwrap()
    }
    fn run(test: impl FnOnce(&StoreCoordinator<'_, '_, ()>, &RequestSpool<'_>, &Fixture, &Timer)) {
        run_with_cells(4, test);
    }
    fn run_with_cells(
        count: usize,
        test: impl FnOnce(&StoreCoordinator<'_, '_, ()>, &RequestSpool<'_>, &Fixture, &Timer),
    ) {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let files = Fixture::new();
        let mut file_root = files.locked();
        let clock = Arc::new(Timer(AtomicU64::new(1)));
        let spool = RequestSpool::open(&mut file_root, clock.clone(), deadline()).unwrap();
        let resources = Limits {
            json_bytes: 64,
            json_methods: 1,
            ..Limits::default()
        }
        .plan()
        .unwrap();
        let plan = DiskLimits {
            response_bytes: 72000,
            response_total_bytes: 144000,
            ..DiskLimits::default()
        }
        .plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )
        .unwrap();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([7; 16]),
            clock.clone(),
            2,
            deadline(),
        )
        .unwrap();
        let mut states: Vec<_> = (0..count).map(|_| SlotState::EMPTY).collect();
        let mut cells: Vec<_> = (0..count).map(|_| LeaseCell::EMPTY).collect();
        let coordinator = StoreCoordinator::new(
            store,
            &plan,
            AuxiliaryUsage {
                sort_bytes: 0,
                response_bytes: 0,
                cache_bytes: 0,
                log_bytes: 0,
                cold_bytes: 0,
            },
            &mut states,
            &mut cells,
            (),
            deadline(),
        )
        .unwrap();
        test(&coordinator, &spool, &files, &clock);
    }
    #[test]
    fn charges_full_caps_and_releases_only_after_owned_file_removal() {
        run(|coordinator, spool, files, _| {
            let first = coordinator.retain_response(spool, deadline()).unwrap();
            let second = coordinator.retain_response(spool, deadline()).unwrap();
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::ResponseBytes)
                    .unwrap(),
                144000
            );
            assert!(matches!(
                coordinator.retain_response(spool, deadline()),
                Err(ResponseError::Ledger(logical::Error::Quota(
                    Kind::ResponseBytes
                )))
            ));
            let path = files.path.join("request-00/response");
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
                0o600
            );
            first.discard().unwrap();
            assert!(!path.exists());
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::ResponseBytes)
                    .unwrap(),
                72000
            );
            drop(second);
            assert_eq!(coordinator.used(Kind::ResponseBytes).unwrap(), 0);
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::ResponseBytes)
                    .unwrap(),
                0
            );
        });
    }
    #[test]
    fn immutable_published_prefix_survives_tail_failure_and_replacement() {
        run(|coordinator, spool, _, _| {
            let mut response = coordinator.retain_response(spool, deadline()).unwrap();
            response.append(b"{\"a\":1}").unwrap();
            let mut output = [0; 7];
            assert_eq!(response.read_at(0, &mut output), Err(ports::Error::Invalid));
            assert_eq!(response.publish().unwrap(), 7);
            response.append(b"incomplete").unwrap();
            response.read_at(0, &mut output).unwrap();
            assert_eq!(&output, b"{\"a\":1}");
            response.rollback_tail().unwrap();
            response.append(b"[]").unwrap();
            assert_eq!(response.publish().unwrap(), 9);
            let mut suffix = [0; 2];
            response.read_at(7, &mut suffix).unwrap();
            assert_eq!(&suffix, b"[]");
            assert_eq!(
                response.read_at(u64::MAX, &mut suffix),
                Err(ports::Error::Capacity)
            );
            response.discard().unwrap();
        });
    }
    #[test]
    fn exact_capacity_rejection_can_rollback_but_does_not_change_published_bytes() {
        run(|coordinator, spool, _, _| {
            let mut response = coordinator.retain_response(spool, deadline()).unwrap();
            response.append(b"[]").unwrap();
            response.publish().unwrap();
            response.append(&vec![b' '; 65536]).unwrap();
            response.append(&vec![b' '; 6462]).unwrap();
            assert_eq!(response.len(), response.maximum());
            assert_eq!(response.append(b"x"), Err(ports::Error::Capacity));
            assert_eq!(response.failure(), None);
            assert_eq!(response.work_failure(), None);
            response.rollback_tail().unwrap();
            assert_eq!(response.len(), 2);
            response.append(b"null").unwrap();
            response.publish().unwrap();
            let mut output = [0; 6];
            response.read_at(0, &mut output).unwrap();
            assert_eq!(&output, b"[]null");
        });
    }
    #[test]
    fn deadline_and_clock_regression_are_sticky_but_cleanup_releases() {
        for end in [0, 100] {
            run(|coordinator, spool, files, clock| {
                let mut response = coordinator.retain_response(spool, deadline()).unwrap();
                response.append(b"[]").unwrap();
                clock.0.store(end, Ordering::SeqCst);
                let failure = response.publish().err().unwrap();
                clock.0.store(1, Ordering::SeqCst);
                assert_eq!(response.append(b"x"), Err(failure));
                response.discard().unwrap();
                assert!(!files.path.join("request-00/response").exists());
                assert_eq!(
                    coordinator
                        .state
                        .lock()
                        .unwrap()
                        .ledger
                        .pending(Kind::ResponseBytes)
                        .unwrap(),
                    0
                );
            });
        }
    }
    #[test]
    fn replaced_file_is_not_deleted_or_credited_and_slot_is_retired() {
        run(|coordinator, spool, files, _| {
            let mut response = coordinator.retain_response(spool, deadline()).unwrap();
            response.append(b"secret").unwrap();
            let path = files.path.join("request-00/response");
            fs::remove_file(&path).unwrap();
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            assert!(response.discard().is_err());
            assert!(path.exists());
            assert_eq!(spool.status().retired_slots, 1);
            assert_eq!(spool.status().occupied_slots, 1);
            assert_eq!(coordinator.used(Kind::ResponseBytes).unwrap(), 72000);
            let next = coordinator.retain_response(spool, deadline()).unwrap();
            assert!(files.path.join("request-01/response").exists());
            assert!(coordinator.retain_response(spool, deadline()).is_err());
            next.discard().unwrap();
            assert_eq!(coordinator.used(Kind::ResponseBytes).unwrap(), 72000);
        });
    }
    #[test]
    fn truncated_published_bytes_are_io_failure_not_missing_reference() {
        run(|coordinator, spool, files, _| {
            let mut response = coordinator.retain_response(spool, deadline()).unwrap();
            response.append(b"[]").unwrap();
            response.publish().unwrap();
            fs::OpenOptions::new()
                .write(true)
                .open(files.path.join("request-00/response"))
                .unwrap()
                .set_len(1)
                .unwrap();
            let error = response.read_at(0, &mut [0; 2]).err().unwrap();
            assert!(matches!(error, ports::Error::Io { .. }));
            assert_eq!(response.append(b"x"), Err(error));
            response.discard().unwrap();
        });
    }
    #[test]
    fn poisoned_coordination_keeps_ticket_pinned_and_refuses_new_admission() {
        run(|coordinator, spool, files, _| {
            let mut response = coordinator.retain_response(spool, deadline()).unwrap();
            response.append(b"[]").unwrap();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = coordinator.state.lock().unwrap();
                panic!("poison coordinator");
            }));
            assert!(response.cleanup().is_err());
            assert!(response.effect.is_some());
            assert!(response.lease.is_some());
            assert!(!files.path.join("request-00/response").exists());
            assert!(coordinator.admission_stopped());
            assert!(coordinator.retain_response(spool, deadline()).is_err());
            let state = match coordinator.state.lock() {
                Ok(_) => panic!("not poisoned"),
                Err(error) => error.into_inner(),
            };
            assert_eq!(state.ledger.pending(Kind::ResponseBytes).unwrap(), 72000);
        });
    }
    #[test]
    fn independent_short_lived_spool_and_symmetric_chunk_errors() {
        run(|coordinator, _, _, clock| {
            let files = Fixture::new();
            let mut root = files.locked();
            let spool = RequestSpool::open(
                &mut root,
                Arc::new(Timer(AtomicU64::new(clock.0.load(Ordering::SeqCst)))),
                deadline(),
            )
            .unwrap();
            let mut response = coordinator.retain_response(&spool, deadline()).unwrap();
            assert_eq!(
                response.append(&vec![0; 65537]),
                Err(ports::Error::Capacity)
            );
            assert_eq!(
                response.read_at(0, &mut vec![0; 65537]),
                Err(ports::Error::Capacity)
            );
            response.append(b"[]").unwrap();
            response.discard().unwrap();
        });
    }
    #[test]
    fn admission_preserves_one_cell_and_releases_failed_spool_reservation() {
        run(|coordinator, spool, _, _| {
            let mut state = coordinator.state.lock().unwrap();
            let mut leases = Vec::new();
            for _ in 0..3 {
                leases.push(
                    state
                        .ledger
                        .reserve(&[[Charge::ZERO; 4]], deadline(), Tick(1))
                        .unwrap(),
                );
            }
            drop(state);
            assert!(matches!(
                coordinator.retain_response(spool, deadline()),
                Err(ResponseError::Ledger(logical::Error::Full))
            ));
            let mut state = coordinator.state.lock().unwrap();
            for lease in leases {
                state.ledger.cancel(lease).unwrap();
            }
            drop(state);
            let owners: Vec<_> = (0..32).map(|_| spool.reserve().unwrap()).collect();
            assert!(matches!(
                coordinator.retain_response(spool, deadline()),
                Err(ResponseError::Store(ports::Error::Busy))
            ));
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::ResponseBytes)
                    .unwrap(),
                0
            );
            drop(owners);
            coordinator
                .retain_response(spool, deadline())
                .unwrap()
                .discard()
                .unwrap();
        });
    }
    #[test]
    fn reads_and_rollbacks_charge_work_and_exhaustion_is_terminal() {
        run(|coordinator, spool, _, _| {
            let mut response = coordinator.retain_response(spool, deadline()).unwrap();
            response.meter = Meter::new(
                deadline(),
                WorkCharge {
                    io_bytes: 4,
                    records: 3,
                    output_bytes: 4,
                    unlinks: 0,
                },
            );
            response.append(b"[]").unwrap();
            response.publish().unwrap();
            response.read_at(0, &mut [0; 2]).unwrap();
            response.rollback_tail().unwrap();
            assert_eq!(
                response.meter.remaining(),
                WorkCharge {
                    io_bytes: 0,
                    records: 0,
                    output_bytes: 2,
                    unlinks: 0,
                }
            );
            assert_eq!(response.rollback_tail(), Err(ports::Error::Capacity));
            assert_eq!(response.failure(), Some(ports::Error::Capacity));
            assert_eq!(
                response.work_failure(),
                Some(crate::admission::work::Stop::Records)
            );
            assert_eq!(response.publish(), Err(ports::Error::Capacity));
            response.discard().unwrap();
        });
    }
    #[test]
    fn invalid_settlement_stops_admission() {
        run(|coordinator, spool, _, _| {
            let mut response = coordinator.retain_response(spool, deadline()).unwrap();
            coordinator
                .state
                .lock()
                .unwrap()
                .ledger
                .complete_effect(response.effect.as_mut().unwrap(), EffectResult::Uncertain)
                .unwrap();
            assert!(response.cleanup().is_err());
            assert!(coordinator.admission_stopped());
            assert!(coordinator.retain_response(spool, deadline()).is_err());
            assert_eq!(coordinator.used(Kind::ResponseBytes).unwrap(), 72000);
        });
    }
    #[test]
    fn cpu_only_work_turns_share_the_original_record_budget() {
        run(|coordinator, spool, _, _| {
            let mut response = coordinator.retain_response(spool, deadline()).unwrap();
            response.meter = Meter::new(
                deadline(),
                WorkCharge {
                    io_bytes: 1000,
                    records: 2,
                    output_bytes: 1000,
                    unlinks: 0,
                },
            );
            response.admit_work_turn().unwrap();
            response.admit_work_turn().unwrap();
            assert_eq!(response.admit_work_turn(), Err(ports::Error::Capacity));
            assert!(response.work_failure().is_some());
            assert_eq!(response.admit_work_turn(), Err(ports::Error::Capacity));
            assert!(matches!(
                response.method_view(
                    AccountId::from_bytes([1; 16]),
                    Deadline::after(Tick(0), 90).unwrap()
                ),
                Err(ports::Error::Capacity)
            ));
            assert_eq!(response.failure(), Some(ports::Error::Capacity));
        });
    }
    #[test]
    fn sort_reservations_share_one_quota_and_leave_response_charges_separate() {
        run(|coordinator, spool, files, _| {
            let maximum = coordinator.sort_bytes;
            assert!(matches!(
                coordinator.retain_sort(spool, 0, deadline()),
                Err(SortError::Store(ports::Error::Invalid))
            ));
            assert!(matches!(
                coordinator.retain_sort(spool, maximum + 1, deadline()),
                Err(SortError::Store(ports::Error::Capacity))
            ));
            assert_eq!(spool.status().occupied_slots, 0);
            let first = coordinator
                .retain_sort(spool, maximum / 2, deadline())
                .unwrap();
            let second = coordinator
                .retain_sort(spool, maximum - maximum / 2, deadline())
                .unwrap();
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes)
                    .unwrap(),
                maximum
            );
            assert!(matches!(
                coordinator.retain_sort(spool, 1, deadline()),
                Err(SortError::Ledger(logical::Error::Quota(Kind::SortBytes)))
            ));
            let response = coordinator.retain_response(spool, deadline()).unwrap();
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::ResponseBytes)
                    .unwrap(),
                coordinator.response_bytes
            );
            assert!(files.path.join("request-02/response").exists());
            first.discard().unwrap();
            assert!(!files.path.join("request-00/response").exists());
            drop(second);
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes)
                    .unwrap(),
                0
            );
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::ResponseBytes)
                    .unwrap(),
                coordinator.response_bytes
            );
            response.discard().unwrap();
            assert_eq!(spool.status().occupied_slots, 0);
            assert_eq!(coordinator.used(Kind::SortBytes).unwrap(), 0);
        });
    }
    #[test]
    fn sort_sealed_prefix_and_reset_preserve_capacity_and_the_original_meter() {
        run(|coordinator, spool, files, _| {
            let mut sort = coordinator.retain_sort(spool, 4, deadline()).unwrap();
            assert!(sort.is_empty());
            assert_eq!(sort.maximum(), 4);
            sort.append(b"ab").unwrap();
            assert_eq!(sort.read_at(0, &mut [0; 2]), Err(ports::Error::Invalid));
            assert_eq!(sort.sealed_len(), 0);
            assert_eq!(sort.seal().unwrap(), 2);
            assert_eq!(sort.sealed_len(), 2);
            assert_eq!(sort.read_at(1, &mut [0; 2]), Err(ports::Error::Invalid));
            sort.append(b"cd").unwrap();
            let mut bytes = [0; 2];
            sort.read_at(0, &mut bytes).unwrap();
            assert_eq!(&bytes, b"ab");
            assert_eq!(sort.read_at(2, &mut bytes), Err(ports::Error::Invalid));
            assert_eq!(sort.append(b"e"), Err(ports::Error::Capacity));
            assert_eq!(sort.failure(), None);
            assert_eq!(sort.seal().unwrap(), 4);
            sort.read_at(2, &mut bytes).unwrap();
            assert_eq!(&bytes, b"cd");
            sort.reset().unwrap();
            assert_eq!(sort.len(), 0);
            assert_eq!(sort.sealed_len(), 0);
            assert_eq!(
                fs::metadata(files.path.join("request-00/response"))
                    .unwrap()
                    .len(),
                0
            );
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes)
                    .unwrap(),
                4
            );
            assert_eq!(sort.read_at(0, &mut bytes), Err(ports::Error::Invalid));
            sort.append(b"xy").unwrap();
            sort.seal().unwrap();
            sort.read_at(0, &mut bytes).unwrap();
            assert_eq!(&bytes, b"xy");
            assert_eq!(
                fs::metadata(files.path.join("request-00/response"))
                    .unwrap()
                    .len(),
                2
            );
            sort.discard().unwrap();
            for budget in [3, 4] {
                let mut sort = coordinator.retain_sort(spool, 4, deadline()).unwrap();
                sort.inner.test_record_budget(budget);
                sort.append(b"a").unwrap();
                sort.seal().unwrap();
                sort.reset().unwrap();
                if budget == 3 {
                    assert_eq!(sort.append(b"b"), Err(ports::Error::Capacity));
                    assert!(sort.work_failure().is_some());
                    assert_eq!(sort.reset(), Err(ports::Error::Capacity));
                } else {
                    sort.append(b"b").unwrap();
                    assert_eq!(sort.len(), 1);
                }
                sort.discard().unwrap();
            }
        });
    }
    #[test]
    fn sort_deadline_and_short_read_failures_are_sticky_but_cleanup_can_finish() {
        run(|coordinator, spool, files, clock| {
            let mut sort = coordinator.retain_sort(spool, 16, deadline()).unwrap();
            sort.append(b"abcd").unwrap();
            sort.seal().unwrap();
            fs::OpenOptions::new()
                .write(true)
                .open(files.path.join("request-00/response"))
                .unwrap()
                .set_len(1)
                .unwrap();
            let error = sort.read_at(0, &mut [0; 4]).unwrap_err();
            assert!(matches!(error, ports::Error::Io { .. }));
            assert_eq!(sort.reset(), Err(error));
            assert_eq!(sort.seal(), Err(error));
            sort.discard().unwrap();
            let mut sort = coordinator.retain_sort(spool, 16, deadline()).unwrap();
            sort.append(b"a").unwrap();
            clock.0.store(100, Ordering::SeqCst);
            assert_eq!(sort.reset(), Err(ports::Error::Deadline));
            assert_eq!(sort.len(), 1);
            assert_eq!(sort.append(b"b"), Err(ports::Error::Deadline));
            assert_eq!(
                fs::metadata(files.path.join("request-00/response"))
                    .unwrap()
                    .len(),
                1
            );
            sort.discard().unwrap();
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes)
                    .unwrap(),
                0
            );
            assert_eq!(spool.status().occupied_slots, 0);
            assert_eq!(coordinator.used(Kind::SortBytes).unwrap(), 0);
        });
    }
    #[test]
    fn uncertain_sort_cleanup_retires_the_slot_and_only_its_sort_charge() {
        run(|coordinator, spool, files, _| {
            let mut sort = coordinator.retain_sort(spool, 16, deadline()).unwrap();
            sort.append(b"x").unwrap();
            let path = files.path.join("request-00/response");
            fs::remove_file(&path).unwrap();
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            assert!(sort.discard().is_err());
            assert!(path.exists());
            assert_eq!(spool.status().retired_slots, 1);
            assert_eq!(coordinator.used(Kind::SortBytes).unwrap(), 16);
            assert_eq!(coordinator.used(Kind::ResponseBytes).unwrap(), 0);
            let response = coordinator.retain_response(spool, deadline()).unwrap();
            response.discard().unwrap();
            assert_eq!(coordinator.used(Kind::SortBytes).unwrap(), 16);
        });
    }
    #[test]
    fn failed_sort_creation_reconciles_its_reservation_without_refunding_response() {
        run(|coordinator, spool, files, _| {
            let response = coordinator.retain_response(spool, deadline()).unwrap();
            fs::remove_dir(files.path.join("request-01")).unwrap();
            assert!(coordinator.retain_sort(spool, 16, deadline()).is_err());
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes)
                    .unwrap(),
                0
            );
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::ResponseBytes)
                    .unwrap(),
                coordinator.response_bytes
            );
            assert_eq!(coordinator.used(Kind::SortBytes).unwrap(), 0);
            assert_eq!(spool.status().occupied_slots, 1);
            assert_eq!(spool.status().retired_slots, 0);
            fs::DirBuilder::new()
                .mode(0o700)
                .create(files.path.join("request-01"))
                .unwrap();
            coordinator
                .retain_sort(spool, 16, deadline())
                .unwrap()
                .discard()
                .unwrap();
            response.discard().unwrap();
        });
    }
    #[test]
    fn sort_slot_exhaustion_preserves_response_slots_and_unwinds_the_new_charge() {
        run_with_cells(64, |coordinator, spool, _, _| {
            let mut sorts = Vec::new();
            for _ in 0..crate::store_fs::SORT_FILE_SLOTS {
                sorts.push(coordinator.retain_sort(spool, 1, deadline()).unwrap());
            }
            let count = u64::from(crate::store_fs::SORT_FILE_SLOTS);
            assert!(matches!(
                coordinator.retain_sort(spool, 1, deadline()),
                Err(SortError::Store(ports::Error::Busy))
            ));
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes)
                    .unwrap(),
                count
            );
            assert_eq!(coordinator.used(Kind::SortBytes).unwrap(), 0);
            assert_eq!(spool.status().occupied_slots, count as u32);
            assert_eq!(spool.status().retired_slots, 0);
            let response = coordinator.retain_response(spool, deadline()).unwrap();
            assert_eq!(spool.status().occupied_slots, count as u32 + 1);
            sorts.clear();
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes)
                    .unwrap(),
                0
            );
            coordinator
                .retain_sort(spool, 1, deadline())
                .unwrap()
                .discard()
                .unwrap();
            response.discard().unwrap();
            assert_eq!(spool.status().occupied_slots, 0);
        });
    }
    #[test]
    fn sort_chunk_bounds_and_stopped_coordinator_refuse_before_effects() {
        run(|coordinator, spool, files, _| {
            let maximum = 2 * crate::limits::SQLITE_BODY_CHUNK_BYTES;
            let mut sort = coordinator
                .retain_sort(spool, maximum as u64, deadline())
                .unwrap();
            let mut bytes = vec![7; crate::limits::SQLITE_BODY_CHUNK_BYTES + 1];
            assert_eq!(sort.append(&bytes), Err(ports::Error::Capacity));
            assert_eq!(sort.len(), 0);
            sort.append(&bytes[..crate::limits::SQLITE_BODY_CHUNK_BYTES])
                .unwrap();
            sort.append(b"x").unwrap();
            sort.seal().unwrap();
            assert_eq!(sort.read_at(0, &mut bytes), Err(ports::Error::Capacity));
            sort.read_at(0, &mut bytes[..crate::limits::SQLITE_BODY_CHUNK_BYTES])
                .unwrap();
            assert!(bytes[..crate::limits::SQLITE_BODY_CHUNK_BYTES]
                .iter()
                .all(|b| *b == 7));
            assert_eq!(sort.failure(), None);
            sort.discard().unwrap();
            coordinator.state.lock().unwrap().stopped = true;
            assert!(matches!(
                coordinator.retain_sort(spool, 1, deadline()),
                Err(SortError::Store(ports::Error::WriterStopped))
            ));
            assert!(!files.path.join("request-00/response").exists());
            assert_eq!(spool.status().occupied_slots, 0);
        });
    }
    struct SortInput {
        remaining: u64,
        ignore_bad_charge: bool,
    }
    impl<const N: usize> SortSource<N> for SortInput {
        fn next(
            &mut self,
            output: &mut [u8; N],
            work: &mut dyn SortWork,
        ) -> Result<crate::external_sort::Next, ports::Error> {
            if self.ignore_bad_charge {
                let _ = work.charge(0, 0);
            } else {
                work.charge(N as u64, 1)?;
            }
            if self.remaining == 0 {
                return Ok(crate::external_sort::Next::Complete);
            }
            self.remaining -= 1;
            output.fill(0);
            output
                .get_mut(..8)
                .ok_or(ports::Error::Invalid)?
                .copy_from_slice(&self.remaining.to_be_bytes());
            Ok(crate::external_sort::Next::Record)
        }
        fn compare(
            &mut self,
            a: &[u8; N],
            b: &[u8; N],
            work: &mut dyn SortWork,
        ) -> Result<std::cmp::Ordering, ports::Error> {
            work.charge(0, 1)?;
            Ok(a.cmp(b))
        }
    }
    fn input(count: u64) -> SortInput {
        SortInput {
            remaining: count,
            ignore_bad_charge: false,
        }
    }
    #[test]
    fn native_sort_uses_two_reserved_files_and_one_resident_workspace() {
        use crate::external_sort::{Progress, Sorter};
        run(|coordinator, spool, _, _| {
            let (mut buffers, mut job) = coordinator
                .sort_job::<8, _>(spool, 2, 200, deadline(), input(19))
                .unwrap();
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes),
                Ok(400)
            );
            let first = coordinator.retain_sort(spool, 200, deadline()).unwrap();
            assert_eq!(
                coordinator.state.lock().unwrap().ledger.available_cells(),
                1
            );
            drop(first);
            assert_eq!(coordinator.used(Kind::SortBytes), Ok(0));
            assert!(matches!(
                coordinator.sort_job::<8, _>(spool, 2, 200, deadline(), input(1)),
                Err(SortError::Store(ports::Error::Busy))
            ));
            let output = {
                let (run, heads) = buffers.buffers();
                let mut sorter = Sorter::new(run, heads).unwrap();
                loop {
                    match sorter.poll(&mut job).unwrap() {
                        Progress::Pending => {}
                        Progress::Complete(output) => break output,
                    }
                }
            };
            assert_eq!(output.records, 19);
            for id in 0..19u64 {
                let mut record = [0; 8];
                job.read_output(output, id, &mut record).unwrap();
                assert_eq!(u64::from_be_bytes(record), id);
            }
            assert_eq!(job.source_mut().remaining, 0);
            drop(buffers);
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes),
                Ok(400)
            );
            drop(job);
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes),
                Ok(0)
            );
            assert_eq!(coordinator.used(Kind::SortBytes), Ok(0));
            let pair = coordinator
                .sort_job::<8, _>(spool, 2, 200, deadline(), input(1))
                .unwrap();
            drop(pair);
        });
    }
    #[test]
    fn native_sort_failure_unwinds_reservations_and_workspace_admission() {
        run_with_cells(3, |coordinator, spool, _, _| {
            let response = coordinator.retain_response(spool, deadline()).unwrap();
            assert_eq!(
                coordinator.state.lock().unwrap().ledger.available_cells(),
                2
            );
            assert!(matches!(
                coordinator.sort_job::<8, _>(spool, 2, 200, deadline(), input(1)),
                Err(SortError::Ledger(logical::Error::Full))
            ));
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes),
                Ok(0)
            );
            assert!(!coordinator.sort_workspace.load(Ordering::Acquire));
            drop(response);
            let pair = coordinator
                .sort_job::<8, _>(spool, 2, 200, deadline(), input(1))
                .unwrap();
            drop(pair);
        });
    }
    #[test]
    fn native_sort_original_job_meter_cannot_be_renewed_by_reset_or_file_switch() {
        use crate::external_sort::{File, Job};
        run(|coordinator, spool, _, _| {
            let (buffers, mut job) = coordinator
                .sort_job::<8, _>(spool, 2, 200, deadline(), input(1))
                .unwrap();
            assert_eq!(
                job.test_remaining_records(),
                coordinator.work.foreground_records - 1
            );
            job.test_record_budget(4);
            Job::<8>::admit(&mut job, 0).unwrap();
            Job::<8>::reset(&mut job, File::A).unwrap();
            Job::<8>::reset(&mut job, File::B).unwrap();
            Job::<8>::reset(&mut job, File::A).unwrap();
            assert_eq!(
                Job::<8>::reset(&mut job, File::B),
                Err(ports::Error::Capacity)
            );
            assert_eq!(job.failure(), Some(ports::Error::Capacity));
            assert_eq!(Job::<8>::admit(&mut job, 0), Err(ports::Error::Capacity));
            drop(job);
            drop(buffers);
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes),
                Ok(0)
            );
        });
    }
    #[test]
    fn native_sort_deadline_and_ignored_source_budget_failure_stay_terminal() {
        use crate::external_sort::Job;
        run(|coordinator, spool, _, timer| {
            let (buffers, mut job) = coordinator
                .sort_job::<8, _>(spool, 2, 200, deadline(), input(1))
                .unwrap();
            timer.0.store(100, Ordering::SeqCst);
            assert_eq!(
                Job::<8>::next(&mut job, &mut [0; 8]),
                Err(ports::Error::Deadline)
            );
            assert_eq!(job.source_mut().remaining, 1);
            drop(job);
            drop(buffers);
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes),
                Ok(0)
            );
        });
        run(|coordinator, spool, _, _| {
            let source = SortInput {
                remaining: 1,
                ignore_bad_charge: true,
            };
            let (buffers, mut job) = coordinator
                .sort_job::<8, _>(spool, 2, 200, deadline(), source)
                .unwrap();
            assert_eq!(
                Job::<8>::next(&mut job, &mut [0; 8]),
                Err(ports::Error::Invalid)
            );
            assert_eq!(job.failure(), Some(ports::Error::Invalid));
            assert_eq!(Job::<8>::admit(&mut job, 0), Err(ports::Error::Invalid));
            drop(job);
            drop(buffers);
        });
    }
    #[test]
    fn native_sort_deterministic_caps_refuse_before_workspace_or_file_effects() {
        run(|coordinator, spool, _, _| {
            for (rows, bytes) in [
                (0, 8),
                (crate::external_sort::MAX_RUN_RECORDS + 1, 8),
                (1, 0),
            ] {
                assert!(matches!(
                    coordinator.sort_job::<8, _>(spool, rows, bytes, deadline(), input(1)),
                    Err(SortError::Store(ports::Error::Invalid))
                ));
            }
            assert!(matches!(
                coordinator.sort_job::<8, _>(
                    spool,
                    1,
                    coordinator.sort_bytes / 2 + 1,
                    deadline(),
                    input(1)
                ),
                Err(SortError::Store(ports::Error::Capacity))
            ));
            assert!(!coordinator.sort_workspace.load(Ordering::Acquire));
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes),
                Ok(0)
            );
        });
    }
    #[test]
    fn native_sort_full_width_runs_and_output_bound_refusal() {
        use crate::external_sort::{Progress, Sorter};
        run(|coordinator, spool, _, _| {
            let (mut buffers, mut job) = coordinator
                .sort_job::<2048, _>(spool, 256, 600_000, deadline(), input(257))
                .unwrap();
            let (run, heads) = buffers.buffers();
            let mut sorter = Sorter::new(run, heads).unwrap();
            let output = loop {
                if let Progress::Complete(output) = sorter.poll(&mut job).unwrap() {
                    break output;
                }
            };
            let mut record = [0; 2048];
            for id in 0..257 {
                job.read_output(output, id, &mut record).unwrap();
                assert_eq!(u64::from_be_bytes(record[..8].try_into().unwrap()), id);
                assert!(record[8..].iter().all(|byte| *byte == 0));
            }
            assert_eq!(
                job.read_output(output, 257, &mut record),
                Err(ports::Error::Invalid)
            );
            assert_eq!(
                job.read_output(output, 0, &mut record),
                Err(ports::Error::Invalid)
            );
        });
    }
    #[test]
    fn native_sort_callback_totals_and_writer_stop_are_enforced() {
        use crate::external_sort::{Job, Next};
        struct Many {
            bytes: bool,
        }
        impl SortSource<8> for Many {
            fn next(
                &mut self,
                _: &mut [u8; 8],
                work: &mut dyn SortWork,
            ) -> Result<Next, ports::Error> {
                if self.bytes {
                    assert_eq!(work.charge(32768, 0), Ok(()));
                    assert_eq!(work.charge(32768, 0), Ok(()));
                    assert_eq!(work.charge(1, 0), Err(ports::Error::Capacity));
                } else {
                    assert_eq!(work.charge(0, 128), Ok(()));
                    assert_eq!(work.charge(0, 128), Ok(()));
                    assert_eq!(work.charge(0, 1), Err(ports::Error::Capacity));
                }
                Ok(Next::Complete)
            }
            fn compare(
                &mut self,
                _: &[u8; 8],
                _: &[u8; 8],
                work: &mut dyn SortWork,
            ) -> Result<std::cmp::Ordering, ports::Error> {
                self.next(&mut [0; 8], work)?;
                Ok(std::cmp::Ordering::Equal)
            }
        }
        for (bytes, compare) in [(false, false), (true, false), (false, true), (true, true)] {
            run(|coordinator, spool, _, _| {
                let (_buffers, mut job) = coordinator
                    .sort_job::<8, _>(spool, 2, 200, deadline(), Many { bytes })
                    .unwrap();
                let result = if compare {
                    Job::<8>::compare(&mut job, &[0; 8], &[0; 8]).map(|_| ())
                } else {
                    Job::<8>::next(&mut job, &mut [0; 8]).map(|_| ())
                };
                assert_eq!(result, Err(ports::Error::Capacity));
                assert_eq!(job.failure(), Some(ports::Error::Capacity));
            });
        }
        run(|coordinator, spool, _, _| {
            let (_buffers, mut job) = coordinator
                .sort_job::<8, _>(spool, 2, 200, deadline(), input(1))
                .unwrap();
            coordinator.state.lock().unwrap().stopped = true;
            assert_eq!(
                Job::<8>::next(&mut job, &mut [0; 8]),
                Err(ports::Error::WriterStopped)
            );
            assert_eq!(job.source_mut().remaining, 1);
            drop(job);
            assert_eq!(coordinator.used(Kind::SortBytes), Ok(0));
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::SortBytes),
                Ok(0)
            );
        });
    }
}
