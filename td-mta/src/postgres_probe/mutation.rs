//! Worker-owned closed mutation transactions with outcome-preserving retirement.
use super::{
    connect_authenticated_until,
    retained::{Bounds, LIFETIME},
    Error, Input, Mode, Request, Worker, MAX_DURATION,
};
use crate::{
    ids::StoreEpoch,
    postgres_receiving::{self as receiving, fanout, Fanout, FanoutOutcome},
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
        deadline: Instant,
        accounts: usize,
    ) -> Result<Self, Error> {
        if !(1..=crate::config::identities::MAX_ACCOUNTS).contains(&accounts) {
            return Err(Error::Invalid);
        }
        Self::open_kind(request, deadline, Some(accounts))
    }
    pub(crate) fn open_renewal(request: &Request<'_>, deadline: Instant) -> Result<Self, Error> {
        Self::open_kind(request, deadline, None)
    }
    fn open_kind(
        request: &Request<'_>,
        deadline: Instant,
        accounts: Option<usize>,
    ) -> Result<Self, Error> {
        if request.mode != Mode::Runtime || super::remaining(deadline)? > MAX_DURATION {
            return Err(Error::Invalid);
        }
        let bounds = Bounds::with_turns(Instant::now(), TURNS)?;
        let mut worker = connect_authenticated_until(
            request,
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
            let receipt =
                postgres_schema::lock_and_inspect(client).map_err(|_| Error::RuntimeSchema)?;
            super::qualify_runtime(client)?;
            Ok(receipt)
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
        self.worker.run(move |client| {
            client.batch("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").map_err(|_| Error::Database)?;
            let row = client.query_one("SELECT m.store_epoch,b.blocked FROM td_mta.schema_migrations m CROSS JOIN td_mta.runtime_barrier b WHERE m.version=1 AND b.singleton=true", &[]).map_err(|_| Error::Database)?;
            let observed: &[u8] = row.try_get(0).map_err(|_| Error::RuntimeSchema)?;
            let blocked: bool = row.try_get(1).map_err(|_| Error::RuntimeMaintenance)?;
            if observed != epoch.as_bytes().as_slice() { return Err(Error::RuntimeSchema); }
            if blocked { return Err(Error::RuntimeMaintenance); }
            Ok(())
        })??;
        Ok(())
    }
    fn finish(&mut self, rollback: bool) -> Result<(), Error> {
        if rollback {
            self.worker
                .run(|client| client.rollback().map_err(|_| Error::Database))??;
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
                .run(move |client| fanout::ready_body(client, &bindings))
                .map_err(receiving::Error::Connection)??;
            self.finish(true).map_err(receiving::Error::Connection)
        })();
        if result.is_err() {
            self.retired = true;
        }
        result
    }
    pub(crate) fn recipient(
        &mut self,
        scope: fanout::RecipientScope,
        declared_bytes: Option<u64>,
        deadline: Instant,
    ) -> Result<crate::smtp_storage::Recipient, receiving::Error> {
        let result = (|| {
            self.begin(scope.epoch, Some(1), deadline)
                .map_err(receiving::Error::Connection)?;
            let check = self
                .worker
                .run(move |client| fanout::recipient_body(client, scope, declared_bytes))
                .map_err(receiving::Error::Connection)??;
            self.finish(true).map_err(receiving::Error::Connection)?;
            Ok(check)
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
            match self
                .worker
                .run(move |client| fanout::publish_body(client, &fanout, deadline, &marker))
            {
                Ok(result) => result,
                Err(_) if attempted.load(Ordering::Relaxed) => Err(receiving::Error::Indeterminate),
                Err(error) => Err(receiving::Error::Connection(error)),
            }
        })();
        match result {
            Ok(publication) => {
                // Durable evidence survives later deadline or retirement failure.
                if self.finish(false).is_err() {
                    self.retired = true;
                }
                Ok(publication)
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
            match self
                .worker
                .run(move |client| owners::update_many_body(client, &values, false, &marker))
            {
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
