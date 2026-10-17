//! One coordinator reservation owns its private file through final unlink.
use super::UploadError;
use crate::{
    admission::{
        logical::{self, Cell as LeaseCell, EffectResult, LeaseId, Leases},
        quota::{Charge, Kind},
        Plan,
    },
    ownership::SlotState,
    ports::{self, Clock, Deadline, Tick, Time},
};
use std::sync::Mutex;
struct AdmissionState<'a> {
    ledger: Leases<'a>,
    stopped: bool,
    recoverable_files: bool,
}
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

/// Process-local response quota only. Own the cold, empty request spool so no
/// second coordinator can account for the same temporary files independently.
pub struct ResponseCoordinator<'sr, 'a> {
    spool: RequestSpool<'sr>,
    state: Mutex<AdmissionState<'a>>,
    clock: std::sync::Arc<dyn Clock>,
    work: crate::admission::WorkLimits,
    maximum: u64,
}
impl<'sr, 'a> ResponseCoordinator<'sr, 'a> {
    pub fn new(
        spool: RequestSpool<'sr>,
        plan: &Plan,
        states: &'a mut [SlotState],
        cells: &'a mut [LeaseCell],
        clock: std::sync::Arc<dyn Clock>,
    ) -> Result<Self, ResponseError> {
        let status = spool.status();
        if status.occupied_slots != 0
            || status.retired_slots != 0
            || cells.len() < 2
            || states.len() < 2
        {
            return Err(ports::Error::Invalid.into());
        }
        let ledger = Leases::new(
            plan,
            crate::admission::quota::Usage::default(),
            states,
            cells,
        )?;
        Ok(Self {
            spool,
            state: Mutex::new(AdmissionState {
                ledger,
                stopped: false,
                recoverable_files: false,
            }),
            clock,
            work: *plan.work(),
            maximum: plan.disk().response_bytes,
        })
    }
    pub fn retain_response<'c>(
        &'c self,
        deadline: Deadline,
    ) -> Result<ResponseRetention<'c, 'c, 'sr, 'a>, ResponseError> {
        RetentionOwner(self).retain(&self.spool, deadline, Kind::ResponseBytes, self.maximum)
    }
    pub fn admission_stopped(&self) -> bool {
        self.state.lock().map_or(true, |state| state.stopped)
    }
    pub fn used(&self) -> Result<u64, logical::Error> {
        self.state
            .lock()
            .map_err(|_| logical::Error::Poisoned)?
            .ledger
            .used(Kind::ResponseBytes)
    }
}

struct RetentionOwner<'c, 'sr, 'a>(&'c ResponseCoordinator<'sr, 'a>);
impl<'c, 'sr, 'a> RetentionOwner<'c, 'sr, 'a> {
    fn state(&self) -> &Mutex<AdmissionState<'a>> {
        &self.0.state
    }
    fn clock(&self) -> &std::sync::Arc<dyn Clock> {
        &self.0.clock
    }
    fn work(&self) -> &crate::admission::WorkLimits {
        &self.0.work
    }

    fn time(&self, deadline: Deadline, last: &mut Tick) -> Result<Time, ports::Error> {
        let time = self.clock().sample()?;
        if time.monotonic < *last {
            return Err(ports::Error::Invalid);
        }
        *last = time.monotonic;
        if deadline.expired(time.monotonic) {
            return Err(ports::Error::Deadline);
        }
        Ok(time)
    }
}

/// Raw bounded retention; publication and JSON validity belong to the dispatcher.
/// No path, file descriptor, mutable ledger or cleanup proof escapes this owner.
#[must_use = "retained response storage remains charged until discarded"]
pub struct ResponseRetention<'c, 's, 'sr, 'a> {
    owner: RetentionOwner<'c, 'sr, 'a>,
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
/// Prepaid unpublished tail; constructed and consumed only by this owner.
pub(crate) struct MutationOutput {
    start: u64,
    maximum: usize,
}
impl<'c, 'sr, 'a> RetentionOwner<'c, 'sr, 'a> {
    fn retain<'s>(
        self,
        spool: &'s RequestSpool<'sr>,
        deadline: Deadline,
        kind: Kind,
        maximum: u64,
    ) -> Result<ResponseRetention<'c, 's, 'sr, 'a>, ResponseError> {
        let mut state = self.state().try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => ports::Error::Busy,
            std::sync::TryLockError::Poisoned(_) => ports::Error::WriterStopped,
        })?;
        if state.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        // Preserve one cell for simultaneous durable publication/maintenance.
        if state.ledger.available_cells() < 2 {
            return Err(logical::Error::Full.into());
        }
        let mut last = self.clock().sample()?.monotonic;
        let now = self.time(deadline, &mut last)?.monotonic;
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
        let work = *self.work();
        let mut response = ResponseRetention {
            owner: self,
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
                    io_bytes: work.foreground_io_bytes,
                    records: work.foreground_records,
                    output_bytes: work.foreground_io_bytes,
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
impl<'c, 's, 'sr, 'a> ResponseRetention<'c, 's, 'sr, 'a> {
    pub(crate) fn method_body_context(
        &mut self,
        phase: Deadline,
    ) -> Result<(std::sync::Arc<dyn Clock>, Meter), ports::Error> {
        if !self.admit_before(phase)? {
            return Err(ports::Error::Deadline);
        }
        Ok((
            self.owner.clock().clone(),
            Meter::new(
                phase,
                WorkCharge {
                    io_bytes: self.owner.work().foreground_io_bytes,
                    records: self.owner.work().foreground_records,
                    output_bytes: self.owner.work().foreground_io_bytes,
                    unlinks: 0,
                },
            ),
        ))
    }

    /// Materialize the complete result bound and prepay its final overwrite.
    /// Any capacity, allocation or initial spool failure precedes store effects.
    pub(crate) fn reserve_mutation_output(
        &mut self,
        maximum: usize,
    ) -> Result<MutationOutput, ports::Error> {
        self.check()?;
        let start = self.length;
        let end = start
            .checked_add(maximum as u64)
            .ok_or(ports::Error::Capacity)?;
        if end > self.maximum {
            return Err(ports::Error::Capacity);
        }
        let zero = [0; 16 * 1024];
        let mut remaining = maximum;
        while remaining > 0 {
            let count = remaining.min(zero.len());
            self.append(zero.get(..count).ok_or(ports::Error::Invalid)?)?;
            self.charge(count, false)?;
            remaining -= count;
        }
        Ok(MutationOutput { start, maximum })
    }
    /// Consume prepaid write credit after a known store result. Only the original
    /// outer clock and physical spool can now prevent retaining that result.
    pub(crate) fn finish_mutation_output(
        &mut self,
        reserved: MutationOutput,
        bytes: &[u8],
    ) -> Result<std::ops::Range<u64>, ports::Error> {
        self.check()?;
        if bytes.len() > reserved.maximum
            || self.length != reserved.start + reserved.maximum as u64
            || self.published > reserved.start
        {
            return Err(ports::Error::Invalid);
        }
        let mut offset = reserved.start;
        for chunk in bytes.chunks(16 * 1024) {
            self.check()?;
            let result = self.temporary()?.write_at(offset, chunk);
            self.record(result)?;
            offset += chunk.len() as u64;
        }
        self.length = offset;
        self.publish()?;
        Ok(reserved.start..offset)
    }

    pub(crate) fn with_mutation_meter<T>(
        &mut self,
        phase: Deadline,
        mutate: impl FnOnce(&mut Meter) -> Result<T, UploadError>,
    ) -> Result<T, UploadError> {
        if !self.admit_before(phase)? {
            return Err(ports::Error::Deadline.into());
        }
        mutate(&mut self.meter)
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
        let result = self.owner.time(self.deadline, &mut self.last).map(|_| ());
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
    /// MIME turns use their own original meter; observe outer/phase freshness
    /// without billing those same turns to the response encoder's record grant.
    pub(crate) fn observe_before(&mut self, phase: ports::Deadline) -> Result<bool, ports::Error> {
        if phase >= self.deadline {
            return Err(ports::Error::Invalid);
        }
        self.check()?;
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
        if bytes.len() > crate::limits::BODY_CHUNK_BYTES || next > self.maximum {
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
        if output.len() > crate::limits::BODY_CHUNK_BYTES {
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
        let mut state = self
            .owner
            .state()
            .lock()
            .map_err(|_| ports::Error::WriterStopped)?;
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

impl Drop for ResponseRetention<'_, '_, '_, '_> {
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
        limits::Limits,
        store_fs::tests::Fixture,
    };
    use std::sync::Arc;
    use std::{
        fs,
        os::unix::fs::OpenOptionsExt,
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
    fn response_only(test: impl FnOnce(&ResponseCoordinator<'_, '_>, &Fixture, &Timer)) {
        response_only_inner(false, test);
    }
    fn response_only_inner(
        retired: bool,
        test: impl FnOnce(&ResponseCoordinator<'_, '_>, &Fixture, &Timer),
    ) {
        let files = Fixture::new();
        let mut root = files.locked();
        let clock = Arc::new(Timer(AtomicU64::new(1)));
        let spool = RequestSpool::open(&mut root, clock.clone(), deadline()).unwrap();
        if retired {
            let mut temporary = spool.reserve().unwrap();
            temporary.create().unwrap();
            let path = files.path.join("request-00/response");
            fs::remove_file(&path).unwrap();
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            drop(temporary);
            assert_eq!(spool.status().retired_slots, 1);
        }
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
        let mut states = [const { SlotState::EMPTY }; 4];
        let mut cells = [const { LeaseCell::EMPTY }; 4];
        let owner = ResponseCoordinator::new(spool, &plan, &mut states, &mut cells, clock.clone());
        if retired {
            assert!(matches!(
                owner,
                Err(ResponseError::Store(ports::Error::Invalid))
            ));
            return;
        }
        let owner = owner.unwrap();
        test(&owner, &files, &clock);
    }
    #[test]
    fn response_only_owner_refuses_a_retired_spool() {
        response_only_inner(true, |_, _, _| panic!("retired spool admitted"));
    }
    #[test]
    fn response_only_owner_preserves_caps_prepaid_output_and_cleanup() {
        response_only(|owner, files, _| {
            let mut first = owner.retain_response(deadline()).unwrap();
            let second = owner.retain_response(deadline()).unwrap();
            assert!(owner.retain_response(deadline()).is_err());
            assert_eq!(
                owner
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::ResponseBytes)
                    .unwrap(),
                144000
            );
            let reserved = first.reserve_mutation_output(400).unwrap();
            assert_eq!(first.finish_mutation_output(reserved, b"{}").unwrap(), 0..2);
            let mut output = [0; 2];
            first.read_at(0, &mut output).unwrap();
            assert_eq!(&output, b"{}");
            first.discard().unwrap();
            drop(second);
            assert_eq!(
                owner
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::ResponseBytes)
                    .unwrap(),
                0
            );
            assert_eq!(owner.used().unwrap(), 0);
            assert_eq!(owner.spool.status().occupied_slots, 0);
            assert!(!files.path.join("request-00/response").exists());
            assert!(!owner.admission_stopped());
        });
    }
    #[test]
    fn response_only_owner_retains_uncertain_cleanup_charges() {
        response_only(|owner, files, _| {
            let mut response = owner.retain_response(deadline()).unwrap();
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
            assert_eq!(owner.used().unwrap(), 72000);
            assert_eq!(owner.spool.status().retired_slots, 1);
            let next = owner.retain_response(deadline()).unwrap();
            assert!(owner.retain_response(deadline()).is_err());
            drop(next);
            assert_eq!(owner.used().unwrap(), 72000);
        });
    }
    #[test]
    fn response_only_owner_keeps_clock_failure_sticky() {
        for end in [0, 100] {
            response_only(|owner, _, clock| {
                let mut response = owner.retain_response(deadline()).unwrap();
                response.append(b"[]").unwrap();
                clock.0.store(end, Ordering::SeqCst);
                let failure = response.publish().err().unwrap();
                clock.0.store(1, Ordering::SeqCst);
                assert_eq!(response.append(b"x"), Err(failure));
                response.discard().unwrap();
                assert_eq!(owner.used().unwrap(), 0);
                assert_eq!(owner.spool.status().occupied_slots, 0);
            });
        }
    }
}
