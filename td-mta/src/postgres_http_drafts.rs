//! One bounded retained draft on the fixed HTTPS body worker.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    device_credentials::Presented,
    draft_input,
    format::Sequence,
    ids::DraftOperationId,
    jmap_request::dispatch::{DraftCreator, DraftError, DraftInput},
    ports::{Clock, CommitFailure, Deadline, Error},
    postgres_drafts::{self as changes, Draft},
    postgres_input::Loaded,
    postgres_probe::{self, Mode},
    postgres_read::ReadSlots,
    postgres_runtime_profile::Binding,
    store_fs::DraftCompletion,
};
use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};
/// One active or uncertain mutation. Retain this owner across HTTP requests.
#[derive(Default)]
pub struct Pending {
    change: Mutex<Option<Draft>>,
}
impl Pending {
    pub fn operation(&self) -> Result<Option<DraftOperationId>, Error> {
        Ok(self
            .change
            .try_lock()
            .map_err(|_| Error::Busy)?
            .as_ref()
            .map(Draft::operation))
    }
}

/// A confirmed historical receipt; settling never authorizes a fresh write.
#[derive(Debug)]
pub struct Settled {
    pub operation: DraftOperationId,
    pub outcome: changes::Outcome,
}
impl Pending {
    /// Use a separate settlement connection allowance, never foreground slots.
    /// Absence or any error retains the exact operation and keeps writes fenced.
    pub fn settle(
        &self,
        loaded: &Loaded,
        executable: &Path,
        settlement_slots: &ReadSlots,
        clock: &RuntimeClock,
        until: Deadline,
    ) -> Result<Option<Settled>, Error> {
        if !executable.is_absolute() {
            return Err(Error::Invalid);
        }
        let mut pending = self.change.try_lock().map_err(|_| Error::Busy)?;
        let Some(change) = pending.as_ref() else {
            return Ok(None);
        };
        let now = Instant::now();
        let deadline = clock.deadline_instant(until)?.min(
            now.checked_add(Duration::from_secs(10))
                .ok_or(Error::Invalid)?,
        );
        if now >= deadline {
            return Err(Error::Deadline);
        }
        let _slot = settlement_slots.acquire().map_err(|_| Error::Busy)?;
        let dns_deadline = deadline.min(
            now.checked_add(Duration::from_secs(5))
                .ok_or(Error::Invalid)?,
        );
        let addresses =
            crate::postgres_resolver::resolve(executable, &loaded.bootstrap, dns_deadline)
                .map_err(|error| match error {
                    crate::postgres_resolver::Error::Deadline => Error::Deadline,
                    _ => Error::Dns,
                })?;
        for address in addresses {
            let mut connection = loaded.bootstrap.request(address);
            connection.mode = Mode::Runtime;
            match changes::inspect(&connection, &loaded.trust, deadline, change) {
                Ok(Some(outcome)) => {
                    let operation = change.operation();
                    *pending = None;
                    return Ok(Some(Settled { operation, outcome }));
                }
                Ok(None) => return Ok(None),
                Err(changes::Error::Connection(postgres_probe::Error::Transport)) => (),
                Err(error) => return Err(map_error(error)),
            }
        }
        Err(Error::Invalid)
    }
}

pub struct Drafts<'a> {
    pub loaded: &'a Loaded,
    pub executable: &'a Path,
    pub binding: &'a Binding,
    pub slots: &'a ReadSlots,
    pub clock: &'a RuntimeClock,
    pub pending: &'a Pending,
    pub password: &'a [u8],
    pub maximum_text: usize,
}
impl DraftCreator for Drafts<'_> {
    fn create_draft(
        &self,
        input: DraftInput<'_>,
        meter: &mut Meter,
    ) -> Result<DraftCompletion, DraftError> {
        self.apply(input, meter)
    }
}
fn store(error: Error) -> DraftError {
    DraftError::Store(error.into())
}
impl Drafts<'_> {
    fn apply(
        &self,
        input: DraftInput<'_>,
        meter: &mut Meter,
    ) -> Result<DraftCompletion, DraftError> {
        if !self.executable.is_absolute()
            || !self.binding.roles().jmap()
            || self.loaded.bootstrap.profile() != self.binding.profile()
            || input.account != self.binding.account()
            || Presented::parse(self.password).is_none_or(|value| value.id() != input.device)
        {
            return Err(store(Error::Forbidden));
        }
        if input
            .expected
            .is_some_and(|(epoch, _)| epoch != self.binding.epoch())
        {
            return Err(store(Error::Conflict));
        }
        let now = self.clock.sample().map_err(store)?.monotonic;
        let original =
            Deadline::after(crate::ports::Tick(0), meter.deadline().tick().0).map_err(store)?;
        let until = Deadline::after(now, 10_000)
            .map_err(store)?
            .min(input.deadline)
            .min(original);
        let deadline = self.clock.deadline_instant(until).map_err(store)?;
        let mut pending = self
            .pending
            .change
            .try_lock()
            .map_err(|_| store(Error::Busy))?;
        if pending.is_some() {
            return Err(store(Error::Busy));
        }
        // One owner retains at most 32 MiB each of source and composed bytes.
        let _slot = self.slots.acquire().map_err(|_| store(Error::Busy))?;
        let parsed = draft_input::parse(input.value, self.maximum_text, self.clock, until, meter)
            .map_err(DraftError::Input)?;
        let dns_deadline = deadline.min(
            Instant::now()
                .checked_add(Duration::from_secs(5))
                .ok_or_else(|| store(Error::Invalid))?,
        );
        let addresses = crate::postgres_resolver::resolve(
            self.executable,
            &self.loaded.bootstrap,
            dns_deadline,
        )
        .map_err(|error| {
            store(match error {
                crate::postgres_resolver::Error::Deadline => Error::Deadline,
                _ => Error::Dns,
            })
        })?;
        let mut loaded_parts = Vec::new();
        loaded_parts
            .try_reserve_exact(parsed.attachments.len())
            .map_err(|_| store(Error::Capacity))?;
        let mut remaining = self.binding.upload_maximum().min(32 * 1024 * 1024) as u64;
        for attachment in &parsed.attachments {
            // The verified body snapshot and fresh authorization use two slots.
            let _authorization_slot = self.slots.acquire().map_err(|_| store(Error::Busy))?;
            let mut result = Err(crate::postgres_body::Error::Missing);
            for address in &addresses {
                // Prepay verification and replay before opening the body snapshot,
                // including transport fallback and protocol overhead.
                meter
                    .charge(
                        self.clock.sample().map_err(store)?.monotonic,
                        Charge {
                            io_bytes: 192 * 1024 + remaining * 3,
                            records: 1024 + remaining.div_ceil(4096) * 3,
                            ..Charge::default()
                        },
                    )
                    .map_err(|stop| {
                        store(if stop == Stop::Deadline {
                            Error::Deadline
                        } else {
                            Error::Capacity
                        })
                    })?;
                let mut connection = self.loaded.bootstrap.request(*address);
                connection.mode = Mode::Runtime;
                result = crate::postgres_body::with_blob(
                    &connection,
                    &self.loaded.trust,
                    deadline,
                    crate::postgres_body::BlobRequest {
                        binding: self.binding,
                        blob: attachment.blob,
                        maximum: remaining,
                    },
                    self.password,
                    |metadata, reader| {
                        let length =
                            usize::try_from(metadata.body.length).map_err(|_| Error::Capacity)?;
                        let mut bytes = Vec::new();
                        bytes
                            .try_reserve_exact(length)
                            .map_err(|_| Error::Capacity)?;
                        bytes.resize(length, 0);
                        let mut offset = 0;
                        while offset < length {
                            if Instant::now() >= deadline {
                                return Err(Error::Deadline);
                            }
                            let end = offset.saturating_add(4096).min(length);
                            let output = bytes.get_mut(offset..end).ok_or(Error::Invalid)?;
                            let n = reader.read_at(offset as u64, output)?;
                            if n == 0 || n > output.len() {
                                return Err(Error::Corrupt);
                            }
                            offset += n;
                        }
                        Ok((
                            changes::AttachmentSource {
                                blob: attachment.blob,
                                length: metadata.body.length,
                                digest: metadata.body.digest,
                            },
                            bytes,
                        ))
                    },
                );
                if !matches!(
                    result,
                    Err(crate::postgres_body::Error::Connection(
                        postgres_probe::Error::Transport
                    ))
                ) {
                    break;
                }
            }
            let (source, bytes) = result.map_err(|error| match error {
                crate::postgres_body::Error::Missing => {
                    DraftError::Input(draft_input::Error::Property("attachments"))
                }
                crate::postgres_body::Error::Unauthenticated
                | crate::postgres_body::Error::Policy => store(Error::Forbidden),
                crate::postgres_body::Error::Capacity => store(Error::Capacity),
                crate::postgres_body::Error::Connection(postgres_probe::Error::Deadline) => {
                    store(Error::Deadline)
                }
                crate::postgres_body::Error::Read(error) => store(error),
                _ => store(Error::Corrupt),
            })?;
            remaining = remaining
                .checked_sub(source.length)
                .ok_or_else(|| store(Error::Capacity))?;
            loaded_parts.push((source, bytes));
        }
        let mut parts = Vec::new();
        parts
            .try_reserve_exact(loaded_parts.len())
            .map_err(|_| store(Error::Capacity))?;
        for (attachment, (source, bytes)) in parsed.attachments.iter().zip(&loaded_parts) {
            parts.push(changes::Attachment {
                source: *source,
                part: crate::compose_text::Attachment {
                    name: attachment.name,
                    content_type: attachment.content_type,
                    description: attachment.description,
                    bytes,
                },
            });
        }
        let draft = changes::prepare_with_attachments(
            changes::DraftRequest {
                binding: self.binding,
                device: input.device,
                mailbox: parsed.mailbox,
                expected: input.expected.map(|(_, sequence)| sequence.number()),
            },
            &parsed.message(),
            &parts,
            deadline,
            meter,
            self.clock,
        )
        .map_err(|error| store(map_error(error)))?;
        meter
            .charge(
                self.clock.sample().map_err(store)?.monotonic,
                Charge {
                    io_bytes: draft.length(),
                    records: draft.length().div_ceil(4096) + 1024,
                    ..Default::default()
                },
            )
            .map_err(|error| {
                store(match error {
                    Stop::Deadline => Error::Deadline,
                    _ => Error::Capacity,
                })
            })?;
        drop(parts);
        drop(loaded_parts);
        let before_epoch = self.binding.epoch();
        let email = draft.email();
        let blob = draft.blob();
        let thread = draft.thread_candidate();
        let size = draft.length();
        *pending = Some(draft);
        let draft = pending.as_ref().ok_or_else(|| store(Error::Invalid))?;
        let mut outcome = Err(changes::Error::Connection(postgres_probe::Error::Transport));
        for address in addresses {
            let mut connection = self.loaded.bootstrap.request(address);
            connection.mode = Mode::Runtime;
            outcome = changes::publish(
                &connection,
                &self.loaded.trust,
                deadline,
                draft,
                self.password,
            );
            if !matches!(
                outcome,
                Err(changes::Error::Connection(postgres_probe::Error::Transport))
            ) {
                break;
            }
        }
        match outcome {
            Ok(outcome) => {
                *pending = None;
                Ok(DraftCompletion {
                    before_epoch,
                    before_sequence: Sequence::from_u64(outcome.old_sequence),
                    email: outcome.email,
                    blob: outcome.blob,
                    thread: outcome.thread,
                    size: outcome.length,
                    outcome: Ok(Sequence::from_u64(outcome.new_sequence)),
                    admission_stopped: false,
                    cleanup_error: None,
                })
            }
            Err(changes::Error::Indeterminate) => Ok(DraftCompletion {
                before_epoch,
                before_sequence: Sequence::from_u64(0),
                email,
                blob,
                thread,
                size,
                outcome: Err(CommitFailure::Indeterminate(Error::WriterStopped)),
                admission_stopped: true,
                cleanup_error: None,
            }),
            Err(error) => {
                *pending = None;
                if error == changes::Error::MissingAttachment {
                    Err(DraftError::Input(draft_input::Error::Property(
                        "attachments",
                    )))
                } else {
                    Err(store(map_error(error)))
                }
            }
        }
    }
}
fn map_error(error: changes::Error) -> Error {
    match error {
        changes::Error::Connection(postgres_probe::Error::Deadline) => Error::Deadline,
        changes::Error::Unauthenticated | changes::Error::Policy => Error::Forbidden,
        changes::Error::State | changes::Error::Epoch => Error::Conflict,
        changes::Error::Capacity => Error::Capacity,
        changes::Error::Quota => Error::Busy,
        changes::Error::Integrity => Error::Corrupt,
        changes::Error::Compose(error) => error,
        changes::Error::MissingMailbox => Error::NotFound,
        _ => Error::Invalid,
    }
}
