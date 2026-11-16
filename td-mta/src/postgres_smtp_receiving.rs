//! Fixed-worker SMTP publication ownership; peer admission remains separate.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    ids::ReceivingOperationId,
    ports::{self, Clock, Deadline},
    postgres_input::Loaded,
    postgres_probe::{self, Endpoint},
    postgres_read::ReadSlots,
    postgres_receiving::{self as changes, Fanout, Message, Target},
    postgres_runtime_profile::Binding,
    postgres_settlement as settlement,
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

/// How long a retained delivery may keep failing inline settlement before
/// the receiving run stops, so a restart releases a fence that cannot heal.
const BACKSTOP: Duration = Duration::from_secs(30);

/// An uncertain delivery, the endpoint it was published to and when the
/// owner took custody of it.
type Retained = (Fanout, Endpoint, Instant);

/// One uncertain delivery, retained across SMTP connections. Runtime
/// startup/recovery wiring belongs to cutover.
pub struct Pending {
    change: Mutex<Option<Retained>>,
    backstop: Duration,
}
impl Default for Pending {
    fn default() -> Self {
        Self::with_backstop(BACKSTOP)
    }
}

#[derive(Debug)]
pub struct Settled {
    pub operation: ReceivingOperationId,
    pub outcome: changes::FanoutOutcome,
}
impl Pending {
    /// Production uses `default`; fixtures shorten the backstop.
    #[doc(hidden)]
    pub fn with_backstop(backstop: Duration) -> Self {
        Self {
            change: Mutex::new(None),
            backstop,
        }
    }
    pub fn operation(&self) -> Result<Option<ReceivingOperationId>, Error> {
        Ok(self
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?
            .as_ref()
            .map(|(change, _, _)| change.operation()))
    }
    /// Historical inspection only, using independent settlement capacity.
    /// A found receipt or proven absence releases the operation; errors
    /// leave it fenced and retained.
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
/// Inspect only at the retained endpoint. Absence proves rollback (POSTGRES.md
/// section 104): no publication can sweep the receipt while the owner is held.
fn settle_held(
    pending: &mut Option<Retained>,
    loaded: &Loaded,
    slots: &ReadSlots,
    deadline: Instant,
) -> Result<Option<Settled>, Error> {
    let Some((change, endpoint, _)) = pending.as_ref() else {
        return Ok(None);
    };
    let _slot = slots.acquire().map_err(|_| ports::Error::Busy)?;
    let connection = settlement::request(loaded, *endpoint);
    let settled = changes::inspect_fanout(&connection, &loaded.trust, deadline, change)
        .map_err(Error::Publication)?
        .map(|outcome| Settled {
            operation: change.operation(),
            outcome,
        });
    *pending = None;
    Ok(settled)
}
/// Settle a retained delivery inline under the held owner lock, capped at
/// three seconds, on a foreground slot released before the caller takes its
/// own. Busy unless the owner is then empty; WriterStopped once a delivery
/// held past `backstop` still cannot settle.
fn release(
    pending: &mut Option<Retained>,
    backstop: Duration,
    loaded: &Loaded,
    slots: &ReadSlots,
    deadline: Instant,
) -> Result<(), ports::Error> {
    if pending.is_some() {
        let inline = settlement::inline_deadline(deadline)?;
        if settle_held(pending, loaded, slots, inline).is_err() {
            let held = pending.as_ref().map_or(Duration::ZERO, |(_, _, since)| {
                Instant::now().saturating_duration_since(*since)
            });
            return Err(if held > backstop {
                ports::Error::WriterStopped
            } else {
                ports::Error::Busy
            });
        }
    }
    Ok(())
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
        let mut pending = self
            .pending
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        let deadline = phase(self.clock, until)?;
        release(
            &mut pending,
            self.pending.backstop,
            self.loaded,
            self.slots,
            deadline,
        )?;
        let _slot = self.slots.acquire().map_err(|_| ports::Error::Busy)?;
        let mut bindings = Vec::new();
        bindings
            .try_reserve_exact(1 + self.additional.len())
            .map_err(|_| ports::Error::Capacity)?;
        bindings.push(self.binding);
        bindings.extend(self.additional.iter().copied());
        for address in addresses(self.loaded, self.executable, deadline)? {
            let connection = settlement::request(self.loaded, address);
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

    /// The cached route settles here, before its session takes a foreground
    /// slot, so settlement never holds a second one. It prepays one settlement
    /// connection from `meter`.
    pub(crate) fn settle_retained(
        &self,
        deadline: Instant,
        meter: &mut Meter,
    ) -> Result<(), Error> {
        let mut pending = self
            .pending
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            settlement::charge(meter, self.clock, 1)?;
            release(
                &mut pending,
                self.pending.backstop,
                self.loaded,
                self.slots,
                deadline,
            )?;
        }
        Ok(())
    }

    /// Within a cached session; a retained delivery was settled beforehand
    /// (`settle_retained`) or still refuses as busy.
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
            // A cached session already holds a foreground slot.
            if session.is_some() {
                return Err(ports::Error::Busy.into());
            }
            settlement::charge(meter, self.clock, 1)?;
            release(
                &mut pending,
                self.pending.backstop,
                self.loaded,
                self.slots,
                deadline,
            )?;
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
        let mut timing = crate::performance::Timer::start(crate::performance::Phase::SmtpPublish);
        let mut endpoint = None;
        let outcome = if let Some(session) = session {
            endpoint = Some(session.endpoint());
            session.publish(&change, deadline)
        } else {
            let mut outcome = Err(changes::Error::Connection(postgres_probe::Error::Transport));
            for address in addresses {
                endpoint = Some(address);
                let connection = settlement::request(self.loaded, address);
                outcome =
                    changes::publish_fanout(&connection, &self.loaded.trust, deadline, &change);
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
        if let (Err(changes::Error::Indeterminate), Some(endpoint)) = (&outcome, endpoint) {
            *pending = Some((change, endpoint, Instant::now()));
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
