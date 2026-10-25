//! One fixed HTTPS authentication worker's finite, closed read transactions.
use super::{connect_authenticated_until, Error, Input, Mode, Request, Worker, MAX_DURATION};
use crate::{
    device_credentials::Presented, http_auth::Principal, ids::StoreEpoch, ports,
    postgres_runtime_profile::Binding, postgres_schema, postgres_snapshot,
};
use std::{
    thread::{self, ThreadId},
    time::{Duration, Instant},
};
use td_crypto::TrustStore;

pub(super) const TURNS: usize = 64;
const LIFETIME: Duration = Duration::from_secs(60);

struct Bounds {
    owner: ThreadId,
    expires: Instant,
    left: usize,
}
impl Bounds {
    fn new(now: Instant) -> Result<Self, Error> {
        Ok(Self {
            owner: thread::current().id(),
            expires: now.checked_add(LIFETIME).ok_or(Error::Invalid)?,
            left: TURNS,
        })
    }
    fn needs_retirement(&self, now: Instant) -> bool {
        self.left == 0 || self.expires.saturating_duration_since(now) <= MAX_DURATION
    }
    fn reusable(&self, deadline: Instant) -> bool {
        self.left != 0 && deadline <= self.expires && self.owner == thread::current().id()
    }
    fn admit(&mut self, now: Instant, deadline: Instant) -> Result<(), Error> {
        if self.owner != thread::current().id() {
            return Err(Error::Invalid);
        }
        if now >= deadline || deadline > self.expires {
            return Err(Error::Deadline);
        }
        if deadline.duration_since(now) > MAX_DURATION {
            return Err(Error::Invalid);
        }
        self.left = self.left.checked_sub(1).ok_or(Error::Invalid)?;
        Ok(())
    }
}

// The mutex in Authentication supplies exclusive access, and Bounds refuses
// polling on any thread other than the one whose waker opened this session.
pub(crate) struct AuthenticationSession {
    worker: Worker,
    bounds: Bounds,
    retired: bool,
    clean_idle: bool,
    epoch: StoreEpoch,
}
impl AuthenticationSession {
    pub(crate) fn open(
        request: &Request<'_>,
        trust: &TrustStore,
        deadline: Instant,
    ) -> Result<Self, Error> {
        if request.mode != Mode::Runtime {
            return Err(Error::Invalid);
        }
        if super::remaining(deadline)? > MAX_DURATION {
            return Err(Error::Invalid);
        }
        let bounds = Bounds::new(Instant::now())?;
        let mut worker = connect_authenticated_until(
            request,
            trust,
            bounds.expires,
            deadline,
            TURNS * 64 * 1024,
            Input::authentication(),
            LIFETIME,
        )?;
        worker.deadline = deadline;
        let schema = worker.run(|client| {
            Box::pin(async move {
                let receipt = postgres_schema::lock_and_inspect(client)
                    .await
                    .map_err(|_| Error::RuntimeSchema)?;
                super::qualify_runtime(client).await?;
                Ok(receipt)
            })
        })??;
        Ok(Self {
            worker,
            bounds,
            retired: false,
            clean_idle: false,
            epoch: schema.store_epoch,
        })
    }
    pub(crate) fn reusable(&self, deadline: Instant) -> bool {
        !self.retired && self.worker.failed.is_none() && self.bounds.reusable(deadline)
    }
    // Poll only an acknowledged idle session before queuing any job SQL.
    pub(crate) fn check_idle(&mut self, deadline: Instant) -> Result<(), ports::Error> {
        if !self.clean_idle || !self.reusable(deadline) {
            return Err(ports::Error::Invalid);
        }
        self.worker.deadline = deadline;
        self.worker.check().map_err(connection)
    }
    pub(crate) fn needs_retirement(&self, now: Instant) -> bool {
        self.bounds.needs_retirement(now)
    }
    pub(crate) fn retire(mut self, deadline: Instant) {
        if self.clean_idle && !self.retired && self.worker.failed.is_none() {
            // This consumed session can only terminate. The acknowledged job
            // is over; its old deadline must not prevent bounded idle cleanup.
            self.worker.deadline = deadline.min(self.bounds.expires);
        } else {
            self.worker.deadline = self.worker.deadline.min(deadline);
        }
    }
    pub(crate) fn authenticate(
        &mut self,
        binding: &Binding,
        presented: Presented,
        deadline: Instant,
    ) -> Result<Principal, ports::Error> {
        let result = self.turn(binding, presented, deadline);
        if result.is_err() && !(result == Err(ports::Error::Forbidden) && self.clean_idle) {
            // Application failures retire authority without poisoning a healthy
            // protocol driver: its bounded Drop must still send Terminate.
            self.retired = true;
        }
        result
    }
    fn turn(
        &mut self,
        binding: &Binding,
        presented: Presented,
        deadline: Instant,
    ) -> Result<Principal, ports::Error> {
        if self.retired || binding.epoch() != self.epoch {
            return Err(ports::Error::Invalid);
        }
        self.clean_idle = false;
        self.bounds
            .admit(Instant::now(), deadline)
            .map_err(connection)?;
        // Only acknowledged rollback permits a new job allowance.
        // TLS and cumulative byte/frame/turn limits retain the session bounds.
        self.worker.deadline = deadline;
        self.worker.check().map_err(connection)?;
        self.worker.begin_validation_turn().map_err(connection)?;
        let device = presented.id();
        let binding = binding.clone();
        let principal = self
            .worker
            .run(|client| {
                Box::pin(async move {
                    client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000")
                        .await.map_err(|_| ports::Error::Invalid)?;
                    // The session migration lock protects the connect-time ledger
                    // and role qualification. Fresh jobs still fence maintenance
                    // and store replacement within their own read snapshot.
                    let row = client.query_typed_one(
                        "SELECT m.store_epoch,b.blocked FROM td_mta.schema_migrations m CROSS JOIN td_mta.runtime_barrier b WHERE m.version=1 AND b.singleton=true",
                        &[],
                    ).await.map_err(|_| ports::Error::Invalid)?;
                    let epoch: &[u8] = row.try_get(0).map_err(|_| ports::Error::Invalid)?;
                    let blocked: bool = row.try_get(1).map_err(|_| ports::Error::Invalid)?;
                    if epoch != binding.epoch().as_bytes().as_slice() || blocked {
                        return Err(ports::Error::Invalid);
                    }
                    let authenticated = match postgres_snapshot::observe(client, &binding, presented).await {
                        Ok(_) => true,
                        Err(postgres_snapshot::Error::Unauthenticated) => false,
                        Err(_) => return Err(ports::Error::Invalid),
                    };
                    client.batch_execute("ROLLBACK").await.map_err(|_| ports::Error::Invalid)?;
                    Ok(authenticated.then_some(Principal { account: binding.account(), device }))
                })
            })
            .map_err(connection)??;
        self.worker.check_deadline().map_err(connection)?;
        self.worker.finish_validation_turn().map_err(connection)?;
        self.clean_idle = true;
        principal.ok_or(ports::Error::Forbidden)
    }
}
impl Drop for AuthenticationSession {
    fn drop(&mut self) {
        // Service shutdown joins the worker before dropping Authentication.
        // Do not poll a driver whose wakeups target that departed thread.
        if self.bounds.owner != thread::current().id() {
            self.worker.failed = Some(Error::Database);
        }
    }
}
fn connection(error: Error) -> ports::Error {
    match error {
        Error::Deadline => ports::Error::Deadline,
        _ => ports::Error::Invalid,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn new_jobs_keep_the_original_session_lifetime_and_finite_turn_count() {
        let now = Instant::now();
        let mut bounds = Bounds::new(now).unwrap();
        let expiry = bounds.expires;
        for _ in 0..TURNS {
            assert!(bounds.reusable(now + MAX_DURATION));
            bounds.admit(now, now + MAX_DURATION).unwrap();
        }
        assert!(!bounds.reusable(now + MAX_DURATION));
        assert_eq!(bounds.admit(now, now + MAX_DURATION), Err(Error::Invalid));
        assert_eq!(bounds.expires, expiry);
        let mut bounds = Bounds::new(now).unwrap();
        assert!(!bounds.reusable(expiry + Duration::from_nanos(1)));
        assert_eq!(bounds.admit(now, now), Err(Error::Deadline));
        assert_eq!(
            bounds.admit(now, now + MAX_DURATION + Duration::from_nanos(1)),
            Err(Error::Invalid)
        );
        assert_eq!(
            bounds.admit(expiry, expiry + Duration::from_nanos(1)),
            Err(Error::Deadline)
        );
    }

    #[test]
    fn idle_retirement_reserves_cleanup_headroom_without_moving_expiry() {
        let now = Instant::now();
        let mut bounds = Bounds::new(now).unwrap();
        let expires = bounds.expires;
        assert!(!bounds.needs_retirement(now));
        assert!(!bounds.needs_retirement(expires - MAX_DURATION - Duration::from_nanos(1)));
        assert!(bounds.needs_retirement(expires - MAX_DURATION));
        assert!(bounds.needs_retirement(expires + Duration::from_secs(1)));
        bounds.left = 0;
        assert!(bounds.needs_retirement(now));
        assert_eq!(bounds.expires, expires);
    }

    #[test]
    fn another_thread_cannot_admit_or_reuse_the_driver() {
        let now = Instant::now();
        let mut bounds = Bounds::new(now).unwrap();
        thread::scope(|scope| {
            scope
                .spawn(|| {
                    assert!(!bounds.reusable(now + MAX_DURATION));
                    assert_eq!(bounds.admit(now, now + MAX_DURATION), Err(Error::Invalid));
                })
                .join()
                .unwrap();
        });
        assert_eq!(bounds.left, TURNS);
        bounds.admit(now, now + MAX_DURATION).unwrap();
    }
}
