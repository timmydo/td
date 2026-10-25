//! Admitted DATA memory with PostgreSQL publication on the SMTP worker.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    format::{
        row::{ReceiptRecipients, ReceiptTls, SmtpReceipt, MAX_RECEIPT_BYTES},
        Sequence,
    },
    ids::AccountId,
    ports::{Clock, Commit, CommitFailure, Deadline, Error},
    postgres_receiving::{self, Message},
    postgres_smtp_receiving::{self as owner, Receiving},
    smtp_storage::{Begin, Commits, Completion, Failure, Job, Storage},
};
use std::{
    net::IpAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, TryLockError,
    },
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

pub struct Backend<'a> {
    owner: Receiving<'a>,
    clock: Arc<RuntimeClock>,
    generation: u64,
    admitted: [AtomicBool; crate::limits::SMTP_DATA_JOBS],
    stopped: AtomicBool,
    session: Mutex<SessionState<'a>>,
}
struct SessionState<'a> {
    owner: Option<ThreadId>,
    connection: Option<(
        crate::postgres_probe::SmtpSession,
        crate::postgres_read::ReadPermit<'a>,
    )>,
}
impl SessionState<'_> {
    fn close(&mut self, deadline: Instant) {
        if let Some((connection, permit)) = self.connection.take() {
            connection.retire(deadline);
            drop(permit);
        }
    }
}
impl<'a> Backend<'a> {
    /// `generation` names the pinned local TLS/policy generation used by the
    /// connection owner, independently of the persisted profile generation.
    pub fn new(
        owner: Receiving<'a>,
        clock: Arc<RuntimeClock>,
        generation: u64,
    ) -> Result<Self, Error> {
        if generation == 0 || !std::ptr::eq(owner.clock, clock.as_ref()) || !owner.valid() {
            return Err(Error::Invalid);
        }
        Ok(Self {
            owner,
            clock,
            generation,
            admitted: [const { AtomicBool::new(false) }; crate::limits::SMTP_DATA_JOBS],
            stopped: AtomicBool::new(false),
            session: Mutex::new(SessionState {
                owner: None,
                connection: None,
            }),
        })
    }
}
impl Backend<'_> {
    fn maintain_session(&self, closing: bool) -> Result<(), Error> {
        let mut state = self.session.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => Error::Busy,
            TryLockError::Poisoned(_) => Error::Invalid,
        })?;
        if state
            .owner
            .is_some_and(|owner| owner != thread::current().id())
        {
            return Err(Error::Invalid);
        }
        let now = Instant::now();
        if state
            .connection
            .as_ref()
            .is_some_and(|(session, _)| closing || session.needs_retirement(now))
        {
            state.close(
                now.checked_add(Duration::from_millis(100))
                    .ok_or(Error::Invalid)?,
            );
        }
        Ok(())
    }
    fn with_session<T>(
        &self,
        until: Deadline,
        operation: impl FnOnce(
            &mut crate::postgres_probe::SmtpSession,
            Deadline,
        ) -> Result<T, owner::Error>,
    ) -> Result<T, owner::Error> {
        let now = Instant::now();
        let deadline = self.clock.deadline_instant(until)?.min(
            now.checked_add(Duration::from_secs(10))
                .ok_or(Error::Invalid)?,
        );
        if now >= deadline {
            return Err(Error::Deadline.into());
        }
        let mut state = self.session.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => Error::Busy,
            TryLockError::Poisoned(_) => Error::Invalid,
        })?;
        let owner = thread::current().id();
        if state.owner.is_some_and(|prior| prior != owner) {
            return Err(Error::Invalid.into());
        }
        state.owner = Some(owner);
        if state
            .connection
            .as_ref()
            .is_some_and(|(session, _)| !session.reusable(deadline))
        {
            state.close(deadline);
        }
        // This probe precedes the new SQL job; replacement cannot replay it.
        if state
            .connection
            .as_mut()
            .is_some_and(|(session, _)| session.check_idle(deadline).is_err())
        {
            state.close(deadline);
        }
        let reused = state.connection.is_some();
        if state.connection.is_none() {
            let permit = self.owner.slots.acquire().map_err(|_| Error::Busy)?;
            let addresses = crate::postgres_resolver::resolve(
                self.owner.executable,
                &self.owner.loaded.bootstrap,
                deadline.min(
                    now.checked_add(Duration::from_secs(5))
                        .ok_or(Error::Invalid)?,
                ),
            )
            .map_err(|error| match error {
                crate::postgres_resolver::Error::Deadline => Error::Deadline,
                _ => Error::Dns,
            })?;
            let mut connection = None;
            for address in addresses {
                let mut request = self.owner.loaded.bootstrap.request(address);
                request.mode = crate::postgres_probe::Mode::Runtime;
                match crate::postgres_probe::SmtpSession::open(
                    &request,
                    &self.owner.loaded.trust,
                    deadline,
                    1 + self.owner.additional.len(),
                ) {
                    Ok(session) => {
                        connection = Some(session);
                        break;
                    }
                    Err(crate::postgres_probe::Error::Transport) => (),
                    Err(error) => {
                        return Err(owner::Error::Publication(
                            postgres_receiving::Error::Connection(error),
                        ))
                    }
                }
            }
            state.connection = Some((connection.ok_or(Error::Dns)?, permit));
        }
        let cap = self.clock.deadline_before(deadline)?;
        let result = operation(&mut state.connection.as_mut().ok_or(Error::Invalid)?.0, cap);
        if reused {
            crate::performance::record(
                crate::performance::Phase::DbReuse,
                Duration::ZERO,
                if result.is_ok() {
                    crate::performance::Outcome::Success
                } else {
                    crate::performance::Outcome::Failed
                },
            );
        }
        if result.is_err()
            || state
                .connection
                .as_ref()
                .is_some_and(|(session, _)| !session.reusable(deadline))
        {
            state.close(deadline);
        }
        result
    }
}
struct Permit<'a>(&'a AtomicBool);
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
enum Stage {
    Writing,
    Prepared(Deadline),
    Done,
}
pub struct Delivery<'a, 'b> {
    backend: &'a Backend<'b>,
    meter: Meter,
    deadline: Deadline,
    stage: Stage,
    failure: Option<Failure>,
    bytes: Vec<u8>,
    maximum: usize,
    hostname: String,
    ehlo: String,
    reverse: String,
    recipients: Vec<u8>,
    account_recipients: Vec<(AccountId, Vec<u8>)>,
    peer: IpAddr,
    tls: ReceiptTls,
    extended: bool,
    header_bytes: usize,
    received_at: i64,
    // Release capacity after this job's resident allocations have been dropped.
    _permit: Permit<'a>,
}
impl<'b> Storage for Backend<'b> {
    type Job<'a>
        = Delivery<'a, 'b>
    where
        Self: 'a;
    fn maintain(&self) -> Result<(), Error> {
        self.maintain_session(false)
    }
    fn close(&self) -> Result<(), Error> {
        self.maintain_session(true)
    }
    fn capacity(&self) -> u64 {
        self.owner.binding.upload_maximum().min(32 * 1024 * 1024) as u64
    }
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    fn ready(&self, account: AccountId, deadline: Deadline) -> Result<(), Error> {
        if self.owner.binding_for(account).is_none() {
            return Err(Error::Forbidden);
        }
        if self.stopped() {
            return Err(Error::WriterStopped);
        }
        self.owner.ready(deadline).map_err(owner_error)
    }
    fn begin(
        &self,
        input: Begin<'_, '_>,
        _random: &mut td_crypto::SystemEntropy,
    ) -> Result<Self::Job<'_>, Failure> {
        let shared: Arc<dyn Clock> = self.clock.clone();
        if !Arc::ptr_eq(&shared, &input.clock) || input.config_generation != self.generation {
            return Err(Failure::Refused(Error::Forbidden));
        }
        let (settings, envelope, extended) =
            input.session.delivery_context().map_err(Failure::Refused)?;
        let allowance = crate::receiving_headers::trace_allowance(settings.hostname)
            .map_err(Failure::Refused)?;
        if envelope.recipients.is_empty()
            || envelope
                .recipients
                .iter()
                .any(|recipient| self.owner.binding_for(recipient.account).is_none())
            || settings.message_bytes as u64 > self.capacity()
            || settings.trace_bytes < allowance
            || input.header_bytes == 0
            || input.header_bytes > self.owner.binding.header_maximum()
            || input.header_bytes > settings.message_bytes
            || envelope.recipients.len() > self.owner.binding.recipient_maximum()
        {
            return Err(Failure::Refused(Error::Invalid));
        }
        if self.stopped() {
            return Err(Failure::Refused(Error::WriterStopped));
        }
        let permit = self
            .admitted
            .iter()
            .find(|admitted| {
                admitted
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            })
            .map(Permit)
            .ok_or(Failure::Retry)?;
        let limits = self.owner.binding.work();
        let maximum = input.session.incoming_limit();
        let mut meter = Meter::new(
            input.deadline,
            Charge {
                io_bytes: limits.foreground_io_bytes,
                records: limits.foreground_records,
                output_bytes: self.capacity() * 2
                    + 1024 * 1024
                    + crate::limits::SMTP_FANOUT_METADATA_BYTES as u64,
                ..Charge::default()
            },
        );
        // Admit the resident DATA buffer and bounded receipt copies before 354.
        charge(
            &self.clock,
            &mut meter,
            Charge {
                io_bytes: 4 * crate::format::MAX_VALUE_BYTES as u64,
                output_bytes: maximum as u64 + 2 * crate::format::MAX_VALUE_BYTES as u64,
                records: 1,
                ..Charge::default()
            },
        )?;
        self.with_session(input.deadline, |session, cap| {
            self.owner.ready_cached(session, cap)
        })
        .map_err(|error| Failure::Refused(owner_error(error)))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(maximum)
            .map_err(|_| Failure::Refused(Error::Capacity))?;
        let mut recipients = Vec::new();
        recipients
            .try_reserve_exact(MAX_RECEIPT_BYTES)
            .map_err(|_| Failure::Refused(Error::Capacity))?;
        recipients.resize(MAX_RECEIPT_BYTES, 0);
        let mut addresses = Vec::new();
        addresses
            .try_reserve_exact(envelope.recipients.len())
            .map_err(|_| Failure::Refused(Error::Capacity))?;
        addresses.extend(
            envelope
                .recipients
                .iter()
                .map(|recipient| recipient.address.as_str()),
        );
        let length = ReceiptRecipients::encode(&addresses, &mut recipients)
            .map_err(|_| Failure::Refused(Error::Invalid))?
            .encoded()
            .len();
        recipients.truncate(length);
        let mut account_recipients = Vec::new();
        account_recipients
            .try_reserve_exact(crate::config::identities::MAX_ACCOUNTS)
            .map_err(|_| Failure::Refused(Error::Capacity))?;
        for recipient in &envelope.recipients {
            if account_recipients
                .iter()
                .any(|(account, _)| *account == recipient.account)
            {
                continue;
            }
            addresses.clear();
            addresses.extend(
                envelope
                    .recipients
                    .iter()
                    .filter(|entry| entry.account == recipient.account)
                    .map(|entry| entry.address.as_str()),
            );
            let mut encoded = Vec::new();
            encoded
                .try_reserve_exact(MAX_RECEIPT_BYTES)
                .map_err(|_| Failure::Refused(Error::Capacity))?;
            encoded.resize(MAX_RECEIPT_BYTES, 0);
            let length = ReceiptRecipients::encode(&addresses, &mut encoded)
                .map_err(|_| Failure::Refused(Error::Invalid))?
                .encoded()
                .len();
            encoded.truncate(length);
            account_recipients.push((recipient.account, encoded));
        }
        let time = self.clock.sample().map_err(Failure::Refused)?;
        charge(&self.clock, &mut meter, Charge::default())?;
        Ok(Delivery {
            backend: self,
            meter,
            deadline: input.deadline,
            stage: Stage::Writing,
            failure: None,
            bytes,
            maximum,
            hostname: copy(settings.hostname)?,
            ehlo: copy(&envelope.ehlo)?,
            reverse: copy(&envelope.reverse_path)?,
            recipients,
            account_recipients,
            peer: input.peer,
            tls: input.tls,
            extended,
            header_bytes: input.header_bytes,
            received_at: time.utc_ms,
            _permit: permit,
        })
    }
}
impl Delivery<'_, '_> {
    fn write_inner(&mut self, bytes: &[u8]) -> Result<(), Failure> {
        if self.backend.stopped() {
            return Err(Failure::Refused(Error::WriterStopped));
        }
        if !matches!(self.stage, Stage::Writing) || bytes.len() > 1000 || !bytes.ends_with(b"\r\n")
        {
            return Err(Failure::Refused(Error::Invalid));
        }
        charge(
            &self.backend.clock,
            &mut self.meter,
            Charge {
                io_bytes: bytes.len() as u64,
                records: 1,
                ..Charge::default()
            },
        )?;
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > self.maximum)
        {
            return Err(Failure::Refused(Error::Quota));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}
impl Job for Delivery<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> Result<(), Failure> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.write_inner(bytes);
        self.failure = result.err();
        result
    }
    fn prepare(&mut self, deadline: Deadline) -> Result<(), Failure> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.backend.stopped() {
            return Err(Failure::Refused(Error::WriterStopped));
        }
        if !matches!(self.stage, Stage::Writing) {
            return Err(Failure::Refused(Error::Conflict));
        }
        let deadline = deadline.min(self.deadline);
        if deadline.expired(self.backend.clock.monotonic().map_err(Failure::Refused)?) {
            self.failure = Some(Failure::Refused(Error::Deadline));
            return Err(Failure::Refused(Error::Deadline));
        }
        charge(&self.backend.clock, &mut self.meter, Charge::default())?;
        self.stage = Stage::Prepared(deadline);
        Ok(())
    }
    fn commit(&mut self) -> Result<Completion, Failure> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let Stage::Prepared(deadline) = self.stage else {
            return Err(Failure::Refused(Error::Conflict));
        };
        if self.backend.stopped() {
            return Err(Failure::Refused(Error::WriterStopped));
        }
        self.stage = Stage::Done;
        charge(
            &self.backend.clock,
            &mut self.meter,
            Charge {
                io_bytes: self.recipients.len() as u64,
                records: 1,
                ..Charge::default()
            },
        )?;
        let recipients = ReceiptRecipients::decode(&self.recipients)
            .map_err(|_| Failure::Refused(Error::Invalid))?;
        let mut targets = Vec::new();
        targets
            .try_reserve_exact(self.account_recipients.len())
            .map_err(|_| Failure::Refused(Error::Capacity))?;
        for (account, encoded) in &self.account_recipients {
            targets.push(postgres_receiving::Target {
                binding: self
                    .backend
                    .owner
                    .binding_for(*account)
                    .ok_or(Failure::Refused(Error::Forbidden))?,
                recipients: ReceiptRecipients::decode(encoded)
                    .map_err(|_| Failure::Refused(Error::Invalid))?,
            });
        }
        let original = Deadline::after(crate::ports::Tick(0), self.meter.deadline().tick().0)
            .map_err(Failure::Refused)?;
        let result = self
            .backend
            .with_session(deadline.min(original), |session, cap| {
                self.backend.owner.create_fanout_cached(
                    &targets,
                    &Message {
                        bytes: &self.bytes,
                        hostname: &self.hostname,
                        extended: self.extended,
                        header_bytes: self.header_bytes,
                        received_at: self.received_at,
                        receipt: SmtpReceipt {
                            peer: self.peer,
                            gateway: None,
                            tls: self.tls,
                            ehlo: &self.ehlo,
                            reverse_path: &self.reverse,
                            recipients,
                        },
                    },
                    cap,
                    &mut self.meter,
                    session,
                )
            });
        match result {
            Ok(outcome) => Ok(Completion {
                outcome: Commits::new(outcome.accounts.iter().map(|(account, result)| Commit {
                    account: *account,
                    epoch: self.backend.owner.binding.epoch(),
                    sequence: Sequence::from_u64(result.new_sequence),
                }))
                .map_err(CommitFailure::Indeterminate),
                stop: false,
            }),
            Err(owner::Error::Publication(postgres_receiving::Error::Indeterminate)) => {
                self.backend.stopped.store(true, Ordering::Release);
                Ok(Completion {
                    outcome: Err(CommitFailure::Indeterminate(Error::WriterStopped)),
                    stop: true,
                })
            }
            Err(owner::Error::Publication(postgres_receiving::Error::HeaderLimit)) => {
                Err(Failure::HeaderLimit)
            }
            Err(error) => Err(Failure::Refused(owner_error(error))),
        }
    }
}
fn copy(value: &str) -> Result<String, Failure> {
    let mut result = String::new();
    result
        .try_reserve_exact(value.len())
        .map_err(|_| Failure::Refused(Error::Capacity))?;
    result.push_str(value);
    Ok(result)
}
fn charge(clock: &RuntimeClock, meter: &mut Meter, amount: Charge) -> Result<(), Failure> {
    meter
        .charge(clock.monotonic().map_err(Failure::Refused)?, amount)
        .map_err(|error| {
            Failure::Refused(if error == Stop::Deadline {
                Error::Deadline
            } else {
                Error::Capacity
            })
        })
}
fn owner_error(error: owner::Error) -> Error {
    use postgres_receiving::Error as P;
    match error {
        owner::Error::Admission(error) | owner::Error::Publication(P::Input(error)) => error,
        owner::Error::Publication(P::Policy) => Error::Forbidden,
        owner::Error::Publication(P::Capacity) => Error::Capacity,
        owner::Error::Publication(P::Quota) => Error::Quota,
        owner::Error::Publication(P::Indeterminate) => Error::WriterStopped,
        _ => Error::Busy,
    }
}
