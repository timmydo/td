//! Bounded authenticated cancellation owner for the fixed HTTPS worker.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    device_credentials::Presented,
    ids::{AccountId, DeviceId, StoreEpoch, SubmissionCancelId, SubmissionId},
    ports::{self, Clock, Deadline},
    postgres_input::Loaded,
    postgres_probe,
    postgres_read::ReadSlots,
    postgres_route_owners::Claim,
    postgres_runtime_profile::Binding,
    postgres_settlement::{self as settlement, Retained},
    postgres_submission_cancel::{self as changes, Cancellation},
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Admission(ports::Error),
    Publication(changes::Error),
}
impl From<ports::Error> for Error {
    fn from(error: ports::Error) -> Self {
        Self::Admission(error)
    }
}

/// Retain across requests with the endpoint the operation was published to.
/// Only the uncertain operation is needed for historical inspection.
#[derive(Default)]
pub struct Pending {
    change: Mutex<Option<Retained<Cancellation>>>,
}
#[derive(Debug)]
pub struct Settled {
    pub operation: SubmissionCancelId,
    pub outcome: changes::Outcome,
}
impl Pending {
    pub fn operation(&self) -> Result<Option<SubmissionCancelId>, Error> {
        Ok(self
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?
            .as_ref()
            .map(|held| held.change.operation()))
    }
    /// Internal historical settlement using independent connection capacity.
    /// A found receipt or proven absence releases the operation; errors
    /// retain it. This is not client auth.
    pub fn settle(
        &self,
        loaded: &Loaded,
        executable: &Path,
        slots: &ReadSlots,
        clock: &RuntimeClock,
        until: Deadline,
    ) -> Result<Option<Settled>, Error> {
        if !executable.is_absolute() {
            return Err(ports::Error::Invalid.into());
        }
        let mut pending = self.change.try_lock().map_err(|_| ports::Error::Busy)?;
        if pending.is_none() {
            return Ok(None);
        }
        let deadline = phase(clock, until)?;
        settle_held(&mut pending, loaded, slots, deadline)
    }
}
/// Inspect only at the retained endpoint. Frontend and route-owner inspection
/// both take the account lock behind the unknown COMMIT, so absence proves
/// rollback unless another process swept the expired receipt (POSTGRES.md
/// section 165).
fn settle_held(
    pending: &mut Option<Retained<Cancellation>>,
    loaded: &Loaded,
    slots: &ReadSlots,
    deadline: Instant,
) -> Result<Option<Settled>, Error> {
    let Some(held) = pending.as_ref() else {
        return Ok(None);
    };
    let _slot = slots.acquire().map_err(|_| ports::Error::Busy)?;
    let connection = settlement::request(loaded, held.endpoint);
    let settled = changes::inspect(&connection, &loaded.trust, deadline, &held.change)
        .map_err(Error::Publication)?
        .map(|outcome| Settled {
            operation: held.change.operation(),
            outcome,
        });
    *pending = None;
    Ok(settled)
}
pub struct Input {
    pub account: AccountId,
    pub device: DeviceId,
    pub submission: SubmissionId,
    pub expected: Option<(StoreEpoch, u64)>,
    pub deadline: Deadline,
}
pub struct Cancellations<'a> {
    pub loaded: &'a Loaded,
    pub executable: &'a Path,
    pub binding: &'a Binding,
    pub slots: &'a ReadSlots,
    pub clock: &'a RuntimeClock,
    pub pending: &'a Pending,
    pub password: &'a [u8],
}
impl Cancellations<'_> {
    /// The trusted relay owner supplies its claim and an idempotent local abort.
    /// The callback must obey the publisher's no-SQL/no-SMTP-round-trip contract;
    /// a connection retry may invoke it again after an aborted transaction.
    pub fn cancel(
        &self,
        input: Input,
        claim: &Claim,
        generation: u64,
        meter: &mut Meter,
        close: impl Fn(Instant) -> Result<(), changes::Error> + Send + Sync + 'static,
    ) -> Result<changes::Outcome, Error> {
        self.publish(input, Some((claim, generation)), meter, close)
    }
    pub fn cancel_frontend(
        &self,
        input: Input,
        meter: &mut Meter,
    ) -> Result<changes::Outcome, Error> {
        self.publish(input, None, meter, |_| Ok(()))
    }
    fn publish(
        &self,
        input: Input,
        owner: Option<(&Claim, u64)>,
        meter: &mut Meter,
        close: impl Fn(Instant) -> Result<(), changes::Error> + Send + Sync + 'static,
    ) -> Result<changes::Outcome, Error> {
        if !self.executable.is_absolute()
            || !self.binding.roles().jmap()
            || self.loaded.bootstrap.profile() != self.binding.profile()
            || input.account != self.binding.account()
            || Presented::parse(self.password).is_none_or(|value| value.id() != input.device)
        {
            return Err(ports::Error::Forbidden.into());
        }
        if input
            .expected
            .is_some_and(|(epoch, _)| epoch != self.binding.epoch())
        {
            return Err(ports::Error::Conflict.into());
        }
        let original = Deadline::after(ports::Tick(0), meter.deadline().tick().0)?;
        let deadline =
            phase(self.clock, input.deadline)?.min(self.clock.deadline_instant(original)?);
        let mut pending = self
            .pending
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if Retained::inspect_before_write(&mut pending, self.binding.account(), Instant::now()) {
            // Settle within this request's capped limits, with a foreground
            // slot it releases before taking its own. A frontend request
            // reports a settlement failure; a route owner's is refused as busy.
            charge(meter, self.clock)?;
            let inline = settlement::inline_deadline(deadline)?;
            let settled = settle_held(&mut pending, self.loaded, self.slots, inline);
            if owner.is_none() {
                settled?;
            }
            if pending.is_some() {
                return Err(ports::Error::Busy.into());
            }
        }
        charge(meter, self.clock)?;
        let _slot = self.slots.acquire().map_err(|_| ports::Error::Busy)?;
        let authorization = changes::ClientRequest {
            binding: self.binding,
            device: input.device,
            expected: input.expected,
        };
        let change = match owner {
            Some((claim, generation)) => {
                changes::prepare_for_device(claim, generation, input.submission, authorization)
            }
            None => changes::prepare_frontend(input.submission, authorization),
        }
        .map_err(Error::Publication)?;
        let addresses = addresses(self.loaded, self.executable, deadline)?;
        let close = Arc::new(close);
        let mut outcome = Err(changes::Error::Connection(postgres_probe::Error::Transport));
        let mut endpoint = None;
        for (index, address) in addresses.into_iter().enumerate() {
            if index != 0 {
                charge(meter, self.clock)?;
            }
            endpoint = Some(address);
            let connection = settlement::request(self.loaded, address);
            let close = close.clone();
            outcome = changes::publish_authenticated(
                &connection,
                &self.loaded.trust,
                deadline,
                &change,
                self.password,
                move |until| close(until),
            );
            if !matches!(
                outcome,
                Err(changes::Error::Connection(postgres_probe::Error::Transport))
            ) {
                break;
            }
        }
        if let (Err(changes::Error::Indeterminate), Some(endpoint)) = (&outcome, endpoint) {
            *pending = Some(Retained::new(change, endpoint, self.binding.account()));
        }
        outcome.map_err(Error::Publication)
    }
}
fn charge(meter: &mut Meter, clock: &RuntimeClock) -> Result<(), ports::Error> {
    // Prepay both receipt inspection and publication connection ceilings,
    // including runtime schema frames, plus whole-group staging/validation.
    meter
        .charge(
            clock.sample()?.monotonic,
            Charge {
                io_bytes: 3 * 1024 * 1024 + 192 * 1024,
                records: 8192,
                ..Default::default()
            },
        )
        .map_err(|error| {
            if error == Stop::Deadline {
                ports::Error::Deadline
            } else {
                ports::Error::Capacity
            }
        })
}
fn phase(clock: &RuntimeClock, until: Deadline) -> Result<Instant, ports::Error> {
    let now = Instant::now();
    let deadline = clock.deadline_instant(until)?.min(
        now.checked_add(Duration::from_secs(10))
            .ok_or(ports::Error::Invalid)?,
    );
    if now >= deadline {
        return Err(ports::Error::Deadline);
    }
    Ok(deadline)
}
fn addresses(
    loaded: &Loaded,
    executable: &Path,
    deadline: Instant,
) -> Result<Vec<crate::postgres_probe::Endpoint>, ports::Error> {
    let deadline = deadline.min(
        Instant::now()
            .checked_add(Duration::from_secs(5))
            .ok_or(ports::Error::Invalid)?,
    );
    crate::postgres_resolver::resolve(executable, &loaded.bootstrap, deadline).map_err(|error| {
        match error {
            crate::postgres_resolver::Error::Deadline => ports::Error::Deadline,
            _ => ports::Error::Dns,
        }
    })
}

impl crate::jmap_request::dispatch::SubmissionCanceler for Cancellations<'_> {
    fn cancel(&self, input: Input, meter: &mut Meter) -> Result<changes::Outcome, Error> {
        self.cancel_frontend(input, meter)
    }
}
