//! Finite worker-owned read transactions under one qualified migration lock.
use super::{connect_authenticated_until, Error, Input, Mode, Request, Worker, MAX_DURATION};
use crate::{ids::StoreEpoch, postgres_schema};
use std::{
    future::Future,
    pin::Pin,
    thread::{self, ThreadId},
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use tokio_postgres::Client;

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

#[derive(Clone, Copy)]
pub(crate) enum ReadIsolation {
    RepeatableReadOnly,
    ReadCommitted,
}

pub(crate) struct ReadSession {
    worker: Worker,
    bounds: Bounds,
    retired: bool,
    clean_idle: bool,
    epoch: StoreEpoch,
}
impl ReadSession {
    pub(crate) fn open(
        request: &Request<'_>,
        trust: &TrustStore,
        deadline: Instant,
    ) -> Result<Self, Error> {
        if request.mode != Mode::Runtime || super::remaining(deadline)? > MAX_DURATION {
            return Err(Error::Invalid);
        }
        let bounds = Bounds::new(Instant::now())?;
        let mut worker = connect_authenticated_until(
            request,
            trust,
            bounds.expires,
            deadline,
            TURNS * 64 * 1024,
            Input::retained(),
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
    pub(crate) fn check_idle(&mut self, deadline: Instant) -> Result<(), Error> {
        if !self.clean_idle || !self.reusable(deadline) {
            return Err(Error::Invalid);
        }
        self.worker.deadline = deadline;
        self.worker.check()
    }
    pub(crate) fn needs_retirement(&self, now: Instant) -> bool {
        self.bounds.needs_retirement(now)
    }
    pub(crate) fn retire(mut self, deadline: Instant) {
        self.worker.deadline = if self.clean_idle && !self.retired && self.worker.failed.is_none() {
            deadline.min(self.bounds.expires)
        } else {
            self.worker.deadline.min(deadline)
        };
    }
    // The session owns BEGIN/ROLLBACK. Callers supply closed, nonpublishing SQL;
    // no transaction, row lock or authority survives a completed read job.
    pub(crate) fn read<T, E>(
        &mut self,
        epoch: StoreEpoch,
        isolation: ReadIsolation,
        deadline: Instant,
        operation: impl for<'a> FnOnce(&'a Client) -> Pin<Box<dyn Future<Output = Result<T, E>> + 'a>>
            + 'static,
    ) -> Result<Result<T, E>, Error> {
        let result = self.turn(epoch, isolation, deadline, operation);
        if !matches!(&result, Ok(Ok(_))) {
            self.retired = true;
        }
        result
    }
    fn turn<T, E>(
        &mut self,
        epoch: StoreEpoch,
        isolation: ReadIsolation,
        deadline: Instant,
        operation: impl for<'a> FnOnce(&'a Client) -> Pin<Box<dyn Future<Output = Result<T, E>> + 'a>>
            + 'static,
    ) -> Result<Result<T, E>, Error> {
        if self.retired || epoch != self.epoch {
            return Err(Error::RuntimeSchema);
        }
        self.clean_idle = false;
        self.bounds.admit(Instant::now(), deadline)?;
        self.worker.deadline = deadline;
        self.worker.check()?;
        self.worker.begin_validation_turn()?;
        let result = self.worker.run(move |client| Box::pin(async move {
            let begin = match isolation {
                ReadIsolation::RepeatableReadOnly => "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000",
                ReadIsolation::ReadCommitted => "BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL idle_in_transaction_session_timeout=10000",
            };
            client.batch_execute(begin).await.map_err(|_| Error::Database)?;
            let row = client.query_typed_one(
                "SELECT m.store_epoch,b.blocked FROM td_mta.schema_migrations m CROSS JOIN td_mta.runtime_barrier b WHERE m.version=1 AND b.singleton=true", &[]
            ).await.map_err(|_| Error::Database)?;
            let observed: &[u8] = row.try_get(0).map_err(|_| Error::RuntimeSchema)?;
            let blocked: bool = row.try_get(1).map_err(|_| Error::RuntimeMaintenance)?;
            if observed != epoch.as_bytes().as_slice() { return Err(Error::RuntimeSchema); }
            if blocked { return Err(Error::RuntimeMaintenance); }
            let result = operation(client).await;
            client.batch_execute("ROLLBACK").await.map_err(|_| Error::Database)?;
            Ok(result)
        }))??;
        self.worker.check_deadline()?;
        self.worker.finish_validation_turn()?;
        self.clean_idle = true;
        Ok(result)
    }
}
impl Drop for ReadSession {
    fn drop(&mut self) {
        if self.bounds.owner != thread::current().id() {
            self.worker.failed = Some(Error::Database);
        }
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
