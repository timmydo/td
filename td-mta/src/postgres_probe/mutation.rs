//! Worker-owned closed mutation transactions with outcome-preserving retirement.
use super::{
    connect_authenticated_until,
    retained::{Bounds, LIFETIME},
    Error, Input, Mode, Request, Worker, MAX_DURATION,
};
use crate::{
    ids::StoreEpoch,
    postgres_receiving::{
        self as receiving,
        fanout::{self, Publication},
        Fanout, FanoutOutcome,
    },
    postgres_route_owners as owners,
    postgres_runtime_profile::Binding,
    postgres_schema,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Instant,
};
use td_crypto::TrustStore;

pub(super) const TURNS: usize = 16;

pub(crate) struct MutationSession {
    worker: Worker,
    bounds: Bounds,
    epoch: StoreEpoch,
    // None selects the fixed renewal budget, never SMTP publication credit.
    accounts: Option<usize>,
    retired: bool,
    clean_idle: bool,
}
impl MutationSession {
    pub(crate) fn open_smtp(
        request: &Request<'_>,
        trust: &TrustStore,
        deadline: Instant,
        accounts: usize,
    ) -> Result<Self, Error> {
        if !(1..=crate::config::identities::MAX_ACCOUNTS).contains(&accounts) {
            return Err(Error::Invalid);
        }
        Self::open_kind(request, trust, deadline, Some(accounts))
    }
    pub(crate) fn open_renewal(
        request: &Request<'_>,
        trust: &TrustStore,
        deadline: Instant,
    ) -> Result<Self, Error> {
        Self::open_kind(request, trust, deadline, None)
    }
    fn open_kind(
        request: &Request<'_>,
        trust: &TrustStore,
        deadline: Instant,
        accounts: Option<usize>,
    ) -> Result<Self, Error> {
        if request.mode != Mode::Runtime || super::remaining(deadline)? > MAX_DURATION {
            return Err(Error::Invalid);
        }
        let bounds = Bounds::with_turns(Instant::now(), TURNS)?;
        let mut worker = connect_authenticated_until(
            request,
            trust,
            bounds.expires,
            deadline,
            TURNS * accounts.map_or(128 * 1024, |count| 40 * 1024 * 1024 * count),
            match accounts {
                Some(count) => Input::retained_fanout(count),
                None => Input::retained_mutations(),
            },
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
            epoch: schema.store_epoch,
            accounts,
            retired: false,
            clean_idle: true,
        })
    }
    pub(crate) fn reusable(&self, deadline: Instant) -> bool {
        !self.retired
            && self.clean_idle
            && self.worker.failed.is_none()
            && self.bounds.reusable(deadline)
    }
    pub(crate) fn check_idle(&mut self, deadline: Instant) -> Result<(), Error> {
        if !self.reusable(deadline) {
            return Err(Error::Invalid);
        }
        self.worker.deadline = deadline;
        self.worker.check()
    }
    pub(crate) fn needs_retirement(&self, now: Instant) -> bool {
        self.retired || self.bounds.needs_retirement(now)
    }
    pub(crate) fn retire(mut self, deadline: Instant) {
        self.worker.deadline = if self.clean_idle && !self.retired && self.worker.failed.is_none() {
            deadline.min(self.bounds.expires)
        } else {
            self.worker.deadline.min(deadline)
        };
    }
    fn begin(
        &mut self,
        epoch: StoreEpoch,
        accounts: Option<usize>,
        deadline: Instant,
    ) -> Result<(), Error> {
        if !self.reusable(deadline)
            || epoch != self.epoch
            || !match (accounts, self.accounts) {
                (Some(count), Some(maximum)) => count != 0 && count <= maximum,
                (None, None) => true,
                _ => false,
            }
        {
            return Err(Error::Invalid);
        }
        self.clean_idle = false;
        self.bounds.admit(Instant::now(), deadline)?;
        self.worker.deadline = deadline;
        self.worker.check()?;
        match accounts {
            Some(count) => self
                .worker
                .validation_window
                .as_ref()
                .ok_or(Error::Invalid)?
                .begin_fanout(count)
                .map_err(|_| Error::Database)?,
            None => self.worker.begin_validation_turn()?,
        }
        self.worker.run(move |client| Box::pin(async move {
            client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)?;
            let row = client.query_typed_one("SELECT m.store_epoch,b.blocked FROM td_mta.schema_migrations m CROSS JOIN td_mta.runtime_barrier b WHERE m.version=1 AND b.singleton=true", &[]).await.map_err(|_| Error::Database)?;
            let observed: &[u8] = row.try_get(0).map_err(|_| Error::RuntimeSchema)?;
            let blocked: bool = row.try_get(1).map_err(|_| Error::RuntimeMaintenance)?;
            if observed != epoch.as_bytes().as_slice() { return Err(Error::RuntimeSchema); }
            if blocked { return Err(Error::RuntimeMaintenance); }
            Ok(())
        }))??;
        Ok(())
    }
    fn finish(&mut self, rollback: bool) -> Result<(), Error> {
        if rollback {
            self.worker.run(|client| {
                Box::pin(async move {
                    client
                        .batch_execute("ROLLBACK")
                        .await
                        .map_err(|_| Error::Database)
                })
            })??;
        }
        self.worker.check_deadline()?;
        self.worker.check()?;
        self.worker.finish_validation_turn()?;
        self.clean_idle = true;
        Ok(())
    }
    pub(crate) fn ready(
        &mut self,
        bindings: Vec<Binding>,
        deadline: Instant,
    ) -> Result<(), receiving::Error> {
        let result = (|| {
            let epoch = bindings.first().ok_or(receiving::Error::Integrity)?.epoch();
            self.begin(epoch, Some(bindings.len()), deadline)
                .map_err(receiving::Error::Connection)?;
            self.worker
                .run(move |client| {
                    Box::pin(async move { fanout::ready_body(client, &bindings).await })
                })
                .map_err(receiving::Error::Connection)??;
            self.finish(true).map_err(receiving::Error::Connection)
        })();
        if result.is_err() {
            self.retired = true;
        }
        result
    }
    pub(crate) fn publish(
        &mut self,
        fanout: &Fanout,
        deadline: Instant,
    ) -> Result<FanoutOutcome, receiving::Error> {
        let attempted = Arc::new(AtomicBool::new(false));
        let marker = attempted.clone();
        let result = (|| {
            self.begin(fanout.epoch()?, Some(fanout.count()), deadline)
                .map_err(receiving::Error::Connection)?;
            let fanout = fanout.clone();
            match self.worker.run(move |client| {
                Box::pin(
                    async move { fanout::publish_body(client, &fanout, deadline, &marker).await },
                )
            }) {
                Ok(result) => result,
                Err(_) if attempted.load(Ordering::Relaxed) => Err(receiving::Error::Indeterminate),
                Err(error) => Err(receiving::Error::Connection(error)),
            }
        })();
        match result {
            Ok(publication) => {
                // Durable evidence survives later deadline or retirement failure.
                let rollback = matches!(publication, Publication::Existing(_));
                if self.finish(rollback).is_err() {
                    self.retired = true;
                }
                Ok(publication.outcome())
            }
            Err(error) => {
                self.retired = true;
                Err(error)
            }
        }
    }
    pub(crate) fn renew(
        &mut self,
        values: Vec<(owners::Claim, u64)>,
        deadline: Instant,
    ) -> Result<i64, owners::Error> {
        let attempted = Arc::new(AtomicBool::new(false));
        let marker = attempted.clone();
        let result = (|| {
            let epoch = values
                .first()
                .ok_or(owners::Error::Integrity)?
                .0
                .binding()
                .epoch();
            self.begin(epoch, None, deadline)
                .map_err(owners::Error::Connection)?;
            match self.worker.run(move |client| {
                Box::pin(
                    async move { owners::update_many_body(client, &values, false, &marker).await },
                )
            }) {
                Ok(result) => result,
                Err(_) if attempted.load(Ordering::Relaxed) => Err(owners::Error::Indeterminate),
                Err(error) => Err(owners::Error::Connection(error)),
            }
        })();
        match result {
            Ok(timestamp) => {
                if self.finish(false).is_err() {
                    self.retired = true;
                }
                Ok(timestamp)
            }
            Err(error) => {
                self.retired = true;
                Err(error)
            }
        }
    }
}
impl Drop for MutationSession {
    fn drop(&mut self) {
        if self.bounds.owner != thread::current().id() {
            self.worker.failed = Some(Error::Database);
        }
    }
}
