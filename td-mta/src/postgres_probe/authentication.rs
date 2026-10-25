//! One fixed HTTPS authentication worker's finite, closed read transactions.
use super::{connect_authenticated_until, Error, Input, Mode, Request, Worker, MAX_DURATION};
use crate::{
    device_credentials::Presented, http_auth::Principal, ports, postgres_runtime_profile::Binding,
    postgres_schema, postgres_snapshot,
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
        Ok(Self {
            worker,
            bounds,
            retired: false,
            clean_idle: false,
        })
    }
    pub(crate) fn reusable(&self, deadline: Instant) -> bool {
        !self.retired && self.worker.failed.is_none() && self.bounds.reusable(deadline)
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
        if result.is_err() {
            // Application refusals retire authority without poisoning a healthy
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
        if self.retired {
            return Err(ports::Error::Invalid);
        }
        self.clean_idle = false;
        self.bounds
            .admit(Instant::now(), deadline)
            .map_err(connection)?;
        // Only acknowledged rollback and unlock permit a new job allowance.
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
                    // Acquire before BEGIN: a repeatable-read snapshot created
                    // before the lock could retain a pre-migration ledger.
                    let schema = postgres_schema::lock_and_inspect(client)
                        .await.map_err(|_| ports::Error::Invalid)?;
                    if schema.store_epoch != binding.epoch() {
                        return Err(ports::Error::Invalid);
                    }
                    super::qualify_runtime(client).await.map_err(connection)?;
                    client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000")
                        .await.map_err(|_| ports::Error::Invalid)?;
                    postgres_snapshot::observe(client, &binding, presented)
                        .await.map_err(|error| match error {
                            postgres_snapshot::Error::Unauthenticated => ports::Error::Forbidden,
                            _ => ports::Error::Invalid,
                        })?;
                    client.batch_execute("ROLLBACK").await.map_err(|_| ports::Error::Invalid)?;
                    let unlocked: bool = client.query_typed_one(
                        "SELECT pg_catalog.pg_advisory_unlock_shared($1)",
                        &[(&postgres_schema::MIGRATION_KEY, tokio_postgres::types::Type::INT8)],
                    ).await.map_err(|_| ports::Error::Invalid)?
                        .try_get(0).map_err(|_| ports::Error::Invalid)?;
                    if !unlocked {
                        return Err(ports::Error::Invalid);
                    }
                    Ok(Principal { account: binding.account(), device })
                })
            })
            .map_err(connection)??;
        self.worker.check_deadline().map_err(connection)?;
        self.worker.finish_validation_turn().map_err(connection)?;
        self.clean_idle = true;
        Ok(principal)
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
