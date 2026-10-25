//! Fixed-worker SMTP publication ownership; peer admission remains separate.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    ids::ReceivingOperationId,
    ports::{self, Clock, Deadline},
    postgres_input::Loaded,
    postgres_probe::{self, Mode},
    postgres_read::ReadSlots,
    postgres_receiving::{self as changes, Fanout, Message, Target},
    postgres_runtime_profile::Binding,
};
use std::{
    path::Path,
    sync::Mutex,
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

/// One active or uncertain delivery, retained across SMTP connections.
/// Runtime startup/recovery wiring belongs to cutover.
#[derive(Default)]
pub struct Pending {
    change: Mutex<Option<Fanout>>,
}

#[derive(Debug)]
pub struct Settled {
    pub operation: ReceivingOperationId,
    pub outcome: changes::FanoutOutcome,
}
impl Pending {
    pub fn operation(&self) -> Result<Option<ReceivingOperationId>, Error> {
        Ok(self
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?
            .as_ref()
            .map(Fanout::operation))
    }
    /// Historical inspection only, using independent settlement capacity.
    /// Absence and errors leave the original operation fenced and retained.
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
        let Some(change) = pending.as_ref() else {
            return Ok(None);
        };
        let deadline = phase(clock, until)?;
        let _slot = slots.acquire().map_err(|_| ports::Error::Busy)?;
        for address in addresses(loaded, executable, deadline)? {
            let mut connection = loaded.bootstrap.request(address);
            connection.mode = Mode::Runtime;
            match changes::inspect_fanout(&connection, &loaded.trust, deadline, change) {
                Ok(Some(outcome)) => {
                    let operation = change.operation();
                    *pending = None;
                    return Ok(Some(Settled { operation, outcome }));
                }
                Ok(None) => return Ok(None),
                Err(changes::Error::Connection(postgres_probe::Error::Transport)) => (),
                Err(error) => return Err(Error::Publication(error)),
            }
        }
        Err(Error::Publication(changes::Error::Connection(
            postgres_probe::Error::Transport,
        )))
    }
}

pub struct Receiving<'a> {
    pub loaded: &'a Loaded,
    pub executable: &'a Path,
    pub binding: &'a Binding,
    pub additional: &'a [&'a Binding],
    pub slots: &'a ReadSlots,
    pub clock: &'a RuntimeClock,
    pub pending: &'a Pending,
}
impl Receiving<'_> {
    pub(crate) fn valid(&self) -> bool {
        self.executable.is_absolute()
            && self.binding.roles().receive()
            && self.loaded.bootstrap.profile() == self.binding.profile()
            && self.additional.len() < crate::config::identities::MAX_ACCOUNTS
            && !self.additional.iter().enumerate().any(|(index, binding)| {
                binding.account() == self.binding.account()
                    || !binding.roles().receive()
                    || binding.epoch() != self.binding.epoch()
                    || binding.selection() != self.binding.selection()
                    || binding.profile() != self.binding.profile()
                    || self
                        .additional
                        .iter()
                        .take(index)
                        .any(|prior| prior.account() == binding.account())
            })
    }
    pub fn binding_for(&self, account: crate::ids::AccountId) -> Option<&Binding> {
        std::iter::once(self.binding)
            .chain(self.additional.iter().copied())
            .find(|binding| binding.account() == account)
    }

    /// A bounded startup/reconnect observation. Every delivery still rechecks
    /// current selection and Inbox inside its publication transaction.
    pub fn ready(&self, until: Deadline) -> Result<(), Error> {
        if !self.valid() {
            return Err(ports::Error::Forbidden.into());
        }
        let pending = self
            .pending
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        let deadline = phase(self.clock, until)?;
        let _slot = self.slots.acquire().map_err(|_| ports::Error::Busy)?;
        let mut bindings = Vec::new();
        bindings
            .try_reserve_exact(1 + self.additional.len())
            .map_err(|_| ports::Error::Capacity)?;
        bindings.push(self.binding);
        bindings.extend(self.additional.iter().copied());
        for address in addresses(self.loaded, self.executable, deadline)? {
            let mut connection = self.loaded.bootstrap.request(address);
            connection.mode = Mode::Runtime;
            let ready = changes::ready_many(&connection, &self.loaded.trust, deadline, &bindings);
            match ready {
                Ok(_) => return Ok(()),
                Err(changes::Error::Connection(postgres_probe::Error::Transport)) => (),
                Err(error) => return Err(Error::Publication(error)),
            }
        }
        Err(Error::Publication(changes::Error::Connection(
            postgres_probe::Error::Transport,
        )))
    }

    pub(crate) fn ready_cached(
        &self,
        session: &mut postgres_probe::MutationSession,
        until: Deadline,
    ) -> Result<(), Error> {
        if !self.valid() {
            return Err(ports::Error::Forbidden.into());
        }
        let pending = self
            .pending
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        let mut bindings = Vec::new();
        bindings
            .try_reserve_exact(1 + self.additional.len())
            .map_err(|_| ports::Error::Capacity)?;
        bindings.push(self.binding);
        bindings.extend(self.additional.iter().copied());
        let values = changes::fanout::ready_bindings(&bindings).map_err(Error::Publication)?;
        session
            .ready(values, phase(self.clock, until)?)
            .map_err(Error::Publication)
    }

    /// Publish completed, caller-admitted SMTP input. The trusted caller owns
    /// peer, gateway, recipient and ingress authority; receipt facts are passive.
    pub fn create(
        &self,
        message: &Message<'_>,
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<changes::Outcome, Error> {
        let outcome = self.create_fanout(
            &[Target {
                binding: self.binding,
                recipients: message.receipt.recipients,
            }],
            message,
            until,
            meter,
        )?;
        outcome
            .accounts
            .first()
            .map(|(_, outcome)| *outcome)
            .ok_or(Error::Admission(ports::Error::Invalid))
    }
    pub fn create_fanout(
        &self,
        targets: &[Target<'_>],
        message: &Message<'_>,
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<changes::FanoutOutcome, Error> {
        self.create_fanout_inner(targets, message, until, meter, None)
    }
    pub(crate) fn create_fanout_cached(
        &self,
        targets: &[Target<'_>],
        message: &Message<'_>,
        until: Deadline,
        meter: &mut Meter,
        session: &mut postgres_probe::MutationSession,
    ) -> Result<changes::FanoutOutcome, Error> {
        self.create_fanout_inner(targets, message, until, meter, Some(session))
    }
    fn create_fanout_inner(
        &self,
        targets: &[Target<'_>],
        message: &Message<'_>,
        until: Deadline,
        meter: &mut Meter,
        session: Option<&mut postgres_probe::MutationSession>,
    ) -> Result<changes::FanoutOutcome, Error> {
        if !self.valid()
            || targets
                .iter()
                .any(|target| self.binding_for(target.binding.account()) != Some(target.binding))
            || message.header_bytes > self.binding.header_maximum()
            || message.receipt.recipients.count() as usize > self.binding.recipient_maximum()
        {
            return Err(ports::Error::Forbidden.into());
        }
        let original = Deadline::after(ports::Tick(0), meter.deadline().tick().0)?;
        let deadline = phase(self.clock, until)?.min(self.clock.deadline_instant(original)?);
        meter
            .charge(
                self.clock.sample()?.monotonic,
                Charge {
                    records: 1,
                    ..Default::default()
                },
            )
            .map_err(stop)?;
        let mut pending = self
            .pending
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        // Retain only one prepared message; ingress admission remains external.
        let _slot = if session.is_none() {
            Some(self.slots.acquire().map_err(|_| ports::Error::Busy)?)
        } else {
            None
        };
        let addresses = if session.is_none() {
            addresses(self.loaded, self.executable, deadline)?
        } else {
            Vec::new()
        };
        let change = {
            let mut timing =
                crate::performance::Timer::start(crate::performance::Phase::SmtpPrepare);
            let change = changes::prepare_fanout(targets, message, deadline, meter, self.clock)
                .map_err(Error::Publication)?;
            timing.succeeded();
            change
        };
        let length = change
            .length()
            .checked_mul(change.count() as u64)
            .ok_or(ports::Error::Capacity)?;
        meter
            .charge(
                self.clock.sample()?.monotonic,
                Charge {
                    io_bytes: length,
                    records: length.div_ceil(4096)
                        + u64::from(message.receipt.recipients.count())
                        + 1024,
                    ..Default::default()
                },
            )
            .map_err(stop)?;
        *pending = Some(change);
        let change = pending.as_ref().ok_or(ports::Error::Invalid)?;
        let mut timing = crate::performance::Timer::start(crate::performance::Phase::SmtpPublish);
        let outcome = if let Some(session) = session {
            session.publish(change, deadline)
        } else {
            let mut outcome = Err(changes::Error::Connection(postgres_probe::Error::Transport));
            for address in addresses {
                let mut connection = self.loaded.bootstrap.request(address);
                connection.mode = Mode::Runtime;
                outcome =
                    changes::publish_fanout(&connection, &self.loaded.trust, deadline, change);
                if !matches!(
                    outcome,
                    Err(changes::Error::Connection(postgres_probe::Error::Transport))
                ) {
                    break;
                }
            }
            outcome
        };
        if outcome.is_ok() {
            timing.succeeded();
        }
        drop(timing);
        if !matches!(outcome, Err(changes::Error::Indeterminate)) {
            *pending = None;
        }
        outcome.map_err(Error::Publication)
    }
}
fn stop(error: Stop) -> ports::Error {
    if error == Stop::Deadline {
        ports::Error::Deadline
    } else {
        ports::Error::Capacity
    }
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
) -> Result<Vec<std::net::IpAddr>, ports::Error> {
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
