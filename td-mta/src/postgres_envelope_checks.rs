//! Retained passive observations of an exact untouched Prepared attempt.
use super::{check_locked, Dispatch, Envelope, Error, Observation};
use crate::{
    ports::Clock,
    postgres_probe::{self, Mode, Request, ValidationBudget},
    postgres_route_owners as owners,
    queue_clock::Guard,
};
use std::{
    marker::PhantomData,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Instant,
};
use td_crypto::TrustStore;

/// Thread-bound passive envelope validator. Every successful check acknowledges
/// rollback before returning, with cumulative protocol bounds and the original
/// deadline. Any failure closes permanently. The caller owns connection/work
/// admission and must compare the returned envelope before any SMTP action.
pub struct EnvelopeChecks {
    worker: Option<postgres_probe::Worker>,
    value: Dispatch,
    observation: Observation,
    mode: Mode,
    limit: Instant,
    failed: Option<Error>,
    _thread: PhantomData<Rc<()>>,
}
impl EnvelopeChecks {
    pub fn open(
        request: &Request<'_>,
        trust: &TrustStore,
        deadline: Instant,
        value: &Dispatch,
        clock: Arc<dyn Clock>,
        guard: Arc<Mutex<Guard>>,
    ) -> Result<Self, Error> {
        Self::open_profile(request, trust, deadline, value, clock, guard, None)
    }
    pub(crate) fn transfer(
        request: &Request<'_>,
        trust: &TrustStore,
        deadline: Instant,
        value: &Dispatch,
        clock: Arc<dyn Clock>,
        guard: Arc<Mutex<Guard>>,
        budget: ValidationBudget,
    ) -> Result<Self, Error> {
        Self::open_profile(request, trust, deadline, value, clock, guard, Some(budget))
    }
    fn open_profile(
        request: &Request<'_>,
        trust: &TrustStore,
        deadline: Instant,
        value: &Dispatch,
        clock: Arc<dyn Clock>,
        guard: Arc<Mutex<Guard>>,
        budget: Option<ValidationBudget>,
    ) -> Result<Self, Error> {
        let worker = postgres_probe::validation_worker(request, trust, deadline, budget)
            .map_err(Error::Connection)?;
        Ok(Self {
            worker: Some(worker),
            value: value.clone(),
            observation: (clock, guard),
            mode: request.mode,
            limit: deadline,
            failed: None,
            _thread: PhantomData,
        })
    }
    pub(crate) fn check_until(&mut self, limit: Instant) -> Result<(Envelope, Instant), Error> {
        self.limit = self.limit.min(limit);
        self.check()
    }
    pub fn check(&mut self) -> Result<(Envelope, Instant), Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.turn();
        if let Err(error) = result {
            drop(self.worker.take());
            self.failed = Some(error);
        }
        result
    }
    fn turn(&mut self) -> Result<(Envelope, Instant), Error> {
        let worker = self.worker.as_mut().ok_or(Error::Integrity)?;
        worker
            .constrain_deadline(self.limit)
            .map_err(Error::Connection)?;
        worker.begin_validation_turn().map_err(Error::Connection)?;
        let value = self.value.clone();
        let observation = self.observation.clone();
        let mode = self.mode;
        let deadline = self.limit;
        let (envelope, limit) = worker
            .run(move |client| {
                Box::pin(async move {
                    owners::begin_transaction(client).await?;
                    let schema = crate::postgres_schema::lock_transaction_and_inspect(client)
                        .await
                        .map_err(|error| {
                            if mode == Mode::Runtime {
                                Error::Connection(postgres_probe::Error::RuntimeSchema)
                            } else {
                                Error::Owner(owners::Error::Schema(error))
                            }
                        })?;
                    if schema.store_epoch != value.claim.binding().epoch() {
                        return Err(owners::Error::Epoch.into());
                    }
                    if mode == Mode::Runtime {
                        postgres_probe::qualify_runtime(client)
                            .await
                            .map_err(Error::Connection)?;
                    }
                    if !owners::lock_account_policy(client, &value.claim, true, false).await? {
                        return Err(Error::Policy);
                    }
                    let result = check_locked(client, deadline, &value, Some(&observation)).await?;
                    client
                        .batch_execute("ROLLBACK")
                        .await
                        .map_err(|_| Error::Database)?;
                    Ok(result)
                })
            })
            .map_err(Error::Connection)??;
        if Instant::now() >= limit {
            return Err(Error::NotDue);
        }
        worker.finish_validation_turn().map_err(Error::Connection)?;
        self.limit = self.limit.min(limit);
        Ok((envelope, self.limit))
    }
}
