//! Bounded persistent account leases with one foreground custody owner.
use super::*;
use std::sync::Condvar;

pub(super) struct AccountState {
    pub retry: bool,
    pub active: bool,
    pub notice_at: Instant,
    pub repaired: bool,
}
// Only a complete empty pass increases the wait; no lease or custody lives here.
struct IdlePause(Duration);
impl IdlePause {
    fn new() -> Self {
        Self(Duration::from_millis(250))
    }
    fn after_round(&mut self, active: bool) -> Duration {
        if active {
            self.0 = Duration::from_millis(250);
        }
        let pause = self.0;
        if !active {
            self.0 = self.0.saturating_mul(2).min(Duration::from_secs(2));
        }
        pause
    }
}
// Hold the next account's place across a shared expiry rate-limit refusal.
pub(super) struct ExpiryTurn {
    cursor: usize,
    count: usize,
}
impl ExpiryTurn {
    fn current(&self, index: usize) -> bool {
        self.cursor == index
    }
    pub(super) fn advance(&mut self) {
        self.cursor = (self.cursor + 1) % self.count;
    }
}
#[derive(Debug, Eq, PartialEq)]
pub(super) enum IdlePoll {
    Observed(Option<i64>),
    Deferred,
}
pub(super) fn idle_poll_result(
    result: Result<Option<i64>, crate::postgres_queue_candidates::Error>,
    expiry_turn: Option<&mut ExpiryTurn>,
) -> Result<IdlePoll, crate::postgres_queue_candidates::Error> {
    match result {
        Ok(observed) => Ok(IdlePoll::Observed(observed)),
        Err(crate::postgres_queue_candidates::Error::Busy) => {
            if let Some(turn) = expiry_turn {
                turn.advance();
            }
            Ok(IdlePoll::Deferred)
        }
        Err(error) => Err(error),
    }
}
struct Slot {
    claim: owners::Claim,
    grant: Option<owners::Grant>,
    renewed: bool,
}
// Four simultaneous claim copies cover the registry, renewal snapshot/SQL
// values and foreground custody. Two bounded names live outside each value.
const _: () = assert!(
    crate::config::identities::MAX_ACCOUNTS
        * (4 * (std::mem::size_of::<Slot>() + 256) + std::mem::size_of::<AccountState>() + 128)
        <= crate::limits::RELAY_ACCOUNT_METADATA_BYTES
);
struct Registry {
    slots: Mutex<Vec<Slot>>,
    wake: Condvar,
    done: AtomicBool,
}
struct Finish<'a>(&'a Registry);
impl Drop for Finish<'_> {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
        self.0.wake.notify_all();
    }
}
struct StopOnExit<'a>(&'a Control);
impl Drop for StopOnExit<'_> {
    fn drop(&mut self) {
        self.0.stop();
    }
}
impl Registry {
    fn snapshot(&self) -> Result<Vec<(usize, owners::Claim, owners::Grant)>, Error> {
        let slots = self.slots.lock().map_err(|_| Error::Worker)?;
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(slots.len())
            .map_err(|_| Error::Admission(ports::Error::Capacity))?;
        for (index, slot) in slots.iter().enumerate() {
            if let Some(grant) = slot.grant {
                snapshot.push((index, slot.claim.clone(), grant));
            }
        }
        Ok(snapshot)
    }
    fn active(&self, index: usize) -> Result<Option<(owners::Claim, owners::Grant)>, Error> {
        let slots = self.slots.lock().map_err(|_| Error::Worker)?;
        let slot = slots.get(index).ok_or(Error::Worker)?;
        Ok(slot
            .grant
            .filter(|_| slot.renewed)
            .map(|grant| (slot.claim.clone(), grant)))
    }
    fn acquire(&self, worker: &Worker<'_, '_>, index: usize) -> Result<(), Error> {
        let claim = {
            let slots = self.slots.lock().map_err(|_| Error::Worker)?;
            let slot = slots.get(index).ok_or(Error::Worker)?;
            if slot.grant.is_some() {
                return Ok(());
            }
            slot.claim.clone()
        };
        let grant = match owners::acquire(
            worker.request,
            worker.trust,
            Instant::now() + Duration::from_secs(10),
            &claim,
        ) {
            Ok(grant) => grant,
            Err(
                owners::Error::Busy | owners::Error::Connection(_) | owners::Error::Indeterminate,
            ) => return Ok(()),
            Err(_) => return Err(Error::Database),
        };
        match owners::observe(
            worker.request,
            worker.trust,
            Instant::now() + Duration::from_secs(10),
            &claim,
            grant.generation,
        ) {
            Ok(_) => (),
            Err(owners::Error::Stale) => {
                // This slot has never dispatched. A known expired/released
                // historical claim can be replaced; uncertainty cannot.
                let replacement = database(owners::prepare(claim.binding(), claim.boot()))?;
                let mut slots = self.slots.lock().map_err(|_| Error::Worker)?;
                let slot = slots.get_mut(index).ok_or(Error::Worker)?;
                if slot.claim.operation() != claim.operation() || slot.grant.is_some() {
                    return Err(Error::Custody);
                }
                slot.claim = replacement;
                return Ok(());
            }
            Err(owners::Error::Connection(_)) => return Ok(()),
            Err(_) => return Err(Error::Database),
        }
        let mut slots = self.slots.lock().map_err(|_| Error::Worker)?;
        let slot = slots.get_mut(index).ok_or(Error::Worker)?;
        if slot.claim.operation() != claim.operation() || slot.grant.is_some() {
            return Err(Error::Custody);
        }
        slot.grant = Some(grant);
        slot.renewed = false;
        self.wake.notify_all();
        Ok(())
    }
    fn renew(
        &self,
        worker: &Worker<'_, '_>,
        renewer: Option<&mut owners::Renewer>,
    ) -> Result<i64, Error> {
        let snapshot = self.snapshot()?;
        let leases = leases(&snapshot)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let observed = database(match renewer {
            Some(renewer) => renewer.renew(worker.request, worker.trust, deadline, &leases),
            None => owners::renew_many(worker.request, worker.trust, deadline, &leases),
        })?;
        let mut slots = self.slots.lock().map_err(|_| Error::Worker)?;
        for (index, claim, grant) in snapshot {
            let slot = slots.get_mut(index).ok_or(Error::Worker)?;
            if slot.claim.operation() == claim.operation()
                && slot
                    .grant
                    .is_some_and(|current| current.generation == grant.generation)
            {
                slot.renewed = true;
            }
        }
        Ok(observed)
    }
    fn renewal_worker(&self, worker: &Worker<'_, '_>, control: &Control) -> Result<(), Error> {
        let mut renewer = owners::Renewer::default();
        while !self.done.load(Ordering::Acquire) && !control.stop_requested() {
            let slots = self.slots.lock().map_err(|_| Error::Worker)?;
            if slots
                .iter()
                .all(|slot| slot.grant.is_none() || slot.renewed)
            {
                let (slots, _) = self
                    .wake
                    .wait_timeout(slots, Duration::from_secs(5))
                    .map_err(|_| Error::Worker)?;
                drop(slots);
            } else {
                drop(slots);
            }
            if self.done.load(Ordering::Acquire) || control.stop_requested() {
                break;
            }
            if let Err(error) = self.renew(worker, Some(&mut renewer)) {
                control.log(
                    crate::observability::Kind::RelayStepFailed {
                        stage: crate::observability::RelayStep::LeaseRenewal,
                    },
                    None,
                    None,
                );
                control.stop();
                return Err(error);
            }
        }
        Ok(())
    }
}

pub(super) fn run(
    worker: &Worker<'_, '_>,
    control: &Control,
    ready: &AtomicBool,
) -> Result<(), Error> {
    let mut bytes = [0; 16];
    database(SystemEntropy::try_new())?
        .fill(&mut bytes)
        .map_err(|_| Error::Custody)?;
    let boot = BootId::from_bytes(bytes);
    let mut slots = Vec::new();
    let mut states = Vec::new();
    let count = worker.profile.bindings().count();
    if count == 0 || count > crate::config::identities::MAX_ACCOUNTS {
        return Err(Error::Custody);
    }
    slots
        .try_reserve_exact(count)
        .map_err(|_| Error::Admission(ports::Error::Capacity))?;
    states
        .try_reserve_exact(count)
        .map_err(|_| Error::Admission(ports::Error::Capacity))?;
    for binding in worker.profile.bindings() {
        slots.push(Slot {
            claim: database(owners::prepare(binding, boot))?,
            grant: None,
            renewed: false,
        });
        states.push(AccountState {
            retry: false,
            active: false,
            notice_at: Instant::now(),
            repaired: false,
        });
    }
    let registry = Registry {
        slots: Mutex::new(slots),
        wake: Condvar::new(),
        done: AtomicBool::new(false),
    };
    ready.store(true, Ordering::Release);
    let mut cursor = 0;
    let observed = loop {
        if control.stop_requested() {
            return Ok(());
        }
        registry.acquire(worker, cursor)?;
        if !registry.snapshot()?.is_empty() {
            break registry.renew(worker, None)?;
        }
        cursor = (cursor + 1) % count;
        if cursor == 0 {
            pause(control, Duration::from_millis(250));
        }
    };
    let guard = Arc::new(Mutex::new(database(Guard::new(Time {
        utc_ms: observed,
        monotonic: worker.clock.sample()?.monotonic,
    }))?));
    let slots = database(ReadSlots::new(2))?;
    let settlement = database(ReadSlots::new(1))?;
    let source_slots = database(ReadSlots::new(2))?;
    let pending = Pending::default();
    let dispatch = Dispatches {
        connection: Connection {
            request: worker.request,
            trust: worker.trust,
            slots: &slots,
            clock: worker.clock.clone(),
        },
        guard,
        pending: &pending,
    };
    let mut buffers = (Box::new([0; TLS_WIRE_BYTES]), Box::new([0; TLS_WIRE_BYTES]));
    let mut dns_cache = crate::dns_owner::Cache::default();
    let mut repairing = None;
    let mut idle = IdlePause::new();
    let mut idle_reader =
        crate::postgres_queue_candidates::IdleReader::new(&slots, worker.request, worker.trust);
    let mut active_round = false;
    let mut expiry_turn = ExpiryTurn { cursor: 0, count };
    let result = std::thread::scope(|scope| {
        let renew = std::thread::Builder::new()
            .name("relay-lease".into())
            .spawn_scoped(scope, || {
                let _stop = StopOnExit(control);
                registry.renewal_worker(worker, control)
            })
            .map_err(|_| Error::Worker)?;
        let finish = Finish(&registry);
        let result = (|| {
            while !control.stop_requested() {
                idle_reader.maintain();
                let request = worker.queue.repair_request();
                if request != repairing {
                    for state in &mut states {
                        state.repaired = false;
                    }
                    repairing = request;
                }
                registry.acquire(worker, cursor)?;
                if let Some((claim, grant)) = registry.active(cursor)? {
                    let state = states.get_mut(cursor).ok_or(Error::Worker)?;
                    buffers = worker.schedule_turn(
                        &dispatch,
                        &settlement,
                        &source_slots,
                        &claim,
                        grant.generation,
                        control,
                        state,
                        &mut idle_reader,
                        &mut dns_cache,
                        expiry_turn.current(cursor).then_some(&mut expiry_turn),
                        buffers,
                    )?;
                    active_round |= state.active;
                    if let Some(request) =
                        repairing.filter(|_| states.iter().all(|state| state.repaired))
                    {
                        worker.queue.finish_repair(request, true);
                    }
                } else if expiry_turn.current(cursor) {
                    expiry_turn.advance();
                }
                cursor = (cursor + 1) % count;
                // One bounded foreground turn per account, including Busy.
                if cursor == 0 {
                    pause(control, idle.after_round(active_round));
                    active_round = false;
                }
            }
            Ok(())
        })();
        drop(finish);
        let renewed = renew.join().map_err(|_| Error::Worker)?;
        result.and(renewed)
    });
    idle_reader.close();
    // Close foreground custody and join renewal before one bounded release.
    let snapshot = registry.snapshot()?;
    let leases = leases(&snapshot)?;
    result.and(database(owners::release_many(
        worker.request,
        worker.trust,
        Instant::now() + Duration::from_secs(10),
        &leases,
    )))
}

fn leases(
    snapshot: &[(usize, owners::Claim, owners::Grant)],
) -> Result<Vec<(&owners::Claim, u64)>, Error> {
    let mut leases = Vec::new();
    leases
        .try_reserve_exact(snapshot.len())
        .map_err(|_| Error::Admission(ports::Error::Capacity))?;
    for (_, claim, grant) in snapshot {
        leases.push((claim, grant.generation));
    }
    Ok(leases)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ports::Tick;

    #[test]
    fn empty_rounds_bound_scan_rate_and_activity_restores_prompt_polling() {
        let mut idle = IdlePause::new();
        let mut elapsed = Duration::ZERO;
        let mut scans = 0;
        while elapsed < Duration::from_secs(60) {
            let pause = idle.after_round(false);
            assert!(pause <= Duration::from_secs(2));
            elapsed += pause;
            scans += 1;
        }
        assert!(
            scans <= 34,
            "idle queue scanned {scans} times in one minute"
        );
        // Activity in any account restores the ordinary whole-pass wait.
        for _ in 0..10 {
            assert_eq!(idle.after_round(true), Duration::from_millis(250));
        }
        assert_eq!(idle.after_round(false), Duration::from_millis(250));
        assert_eq!(idle.after_round(false), Duration::from_millis(500));
        assert_eq!(idle.after_round(false), Duration::from_secs(1));
        assert_eq!(idle.after_round(false), Duration::from_secs(2));
    }

    #[test]
    fn busy_idle_account_yields_expiry_to_the_next_account() {
        use crate::postgres_queue_candidates::Error as PollError;
        let sample = |ms| Time {
            utc_ms: ms as i64,
            monotonic: Tick(ms),
        };
        let mut guard = Guard::new(sample(0)).unwrap();
        guard.confirm(sample(300_000)).unwrap();
        let mut turn = ExpiryTurn {
            cursor: 0,
            count: 2,
        };
        let mut expired = Vec::new();
        for cycle in 0..6 {
            for account in 0..2 {
                let owns_expiry = turn.current(account);
                let result = if account == 0 {
                    Err(PollError::Busy)
                } else {
                    Ok(None)
                };
                let observed = idle_poll_result(result, owns_expiry.then_some(&mut turn)).unwrap();
                if account == 0 {
                    assert_eq!(observed, IdlePoll::Deferred);
                } else {
                    assert_eq!(observed, IdlePoll::Observed(None));
                    if owns_expiry {
                        guard.admit_expiry(sample(300_000 + cycle * 1000)).unwrap();
                        expired.push(account);
                        turn.advance();
                    }
                }
            }
        }
        assert_eq!(expired, vec![1; 6]);
        let prior = turn.cursor;
        assert_eq!(
            idle_poll_result(Err(PollError::Policy), Some(&mut turn)),
            Err(PollError::Policy)
        );
        assert_eq!(turn.cursor, prior);
        assert_eq!(
            idle_poll_result(Err(PollError::Busy), None),
            Ok(IdlePoll::Deferred)
        );
        assert_eq!(turn.cursor, prior);
    }

    #[test]
    fn competing_expiry_backlogs_alternate_across_rate_limits() {
        let sample = |ms| Time {
            utc_ms: ms as i64,
            monotonic: Tick(ms),
        };
        let mut guard = Guard::new(sample(0)).unwrap();
        guard.confirm(sample(300_000)).unwrap();
        let mut turn = ExpiryTurn {
            cursor: 0,
            count: 2,
        };
        let mut admitted = Vec::new();
        for cycle in 0..24 {
            for index in 0..2 {
                if !turn.current(index) {
                    continue;
                }
                match guard.admit_expiry(sample(300_000 + cycle * 270 + index as u64 * 10)) {
                    Ok(_) => {
                        admitted.push(index);
                        turn.advance();
                    }
                    Err(error) => assert_eq!(error, crate::queue_clock::Error::RateLimited),
                }
            }
        }
        assert!(admitted.len() >= 5);
        assert!(admitted.windows(2).all(|pair| pair.first() != pair.get(1)));
        // Accounts without a lease or due work yield their place immediately.
        let previous = turn.cursor;
        turn.advance();
        assert_ne!(previous, turn.cursor);
    }
}
