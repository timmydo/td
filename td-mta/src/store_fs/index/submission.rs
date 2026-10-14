//! Freeze an authorized td-mail envelope and its private outgoing body atomically.
use super::draft::{scratch, ComposeClock, Reservation};
use super::*;
use crate::{
    admission::work::{Charge as WorkCharge, Meter, Stop},
    format::row::{
        AttemptPhase, FailureReason, NotificationState, RecipientRow, RecipientState, SubmissionRow,
    },
    ids::{EmailId, IdentityId, SubmissionId, ThreadId},
    ports::BlobReader,
    root_headers,
    submission_envelope::{self, Envelope},
    transmitted_copy,
};

/// Trusted policy guard for this exact configured sending identity. Holding it
/// excludes identity/config publication and device revocation through COMMIT.
pub trait SubmissionGuard: UploadGuard {
    fn identity(&self) -> IdentityId;
    fn email(&self) -> &str;
}
/// Acquisition obeys the deadline and must not call back into the coordinator.
pub trait SubmissionAuthorization {
    type Guard<'a>: SubmissionGuard
    where
        Self: 'a;
    fn authorize_submission(
        &self,
        account: AccountId,
        device: DeviceId,
        identity: IdentityId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, ports::Error>;
}
#[derive(Clone, Copy, Debug)]
pub struct SubmissionRequest {
    pub account: AccountId,
    pub device: DeviceId,
    pub email: EmailId,
    pub identity: IdentityId,
    pub expected: Option<(StoreEpoch, Sequence)>,
    pub maximum: u64,
    pub header_bytes: u64,
    pub recipients: usize,
    pub deadline: Deadline,
}
#[derive(Debug)]
pub enum SubmissionError {
    Storage(UploadError),
    Envelope(submission_envelope::Error),
}
impl From<ports::Error> for SubmissionError {
    fn from(error: ports::Error) -> Self {
        Self::Storage(error.into())
    }
}
impl From<UploadError> for SubmissionError {
    fn from(error: UploadError) -> Self {
        Self::Storage(error)
    }
}
impl From<logical::Error> for SubmissionError {
    fn from(error: logical::Error) -> Self {
        Self::Storage(error.into())
    }
}
impl From<format::Error> for SubmissionError {
    fn from(error: format::Error) -> Self {
        Self::Storage(error.into())
    }
}
impl From<submission_envelope::Error> for SubmissionError {
    fn from(error: submission_envelope::Error) -> Self {
        Self::Envelope(error)
    }
}
#[derive(Debug)]
#[must_use = "preserve known publication even if accounting or cleanup fails"]
pub struct SubmissionCompletion {
    pub before: ViewIdentity,
    pub submission: SubmissionId,
    pub blob: BlobId,
    pub size: u64,
    pub recipients: u32,
    pub outcome: Result<Sequence, CommitError>,
    pub admission_stopped: bool,
    pub cleanup_error: Option<ports::Error>,
}
struct Source {
    epoch: StoreEpoch,
    blob: BlobId,
    thread: ThreadId,
    access: Access,
}
fn source(
    view: &mut IndexReadView<'_, '_>,
    request: SubmissionRequest,
    value: &mut [u8],
) -> Result<(BlobId, ThreadId), ports::Error> {
    let before = view.identity();
    if request
        .expected
        .is_some_and(|expected| expected != (before.epoch, before.committed_sequence))
    {
        return Err(ports::Error::Conflict);
    }
    match view.get(Key::Email(request.email), value)? {
        Some((Row::Email(row), _)) => Ok((row.blob, row.thread)),
        None => Err(ports::Error::NotFound),
        _ => Err(ports::Error::Corrupt),
    }
}
fn pay(clock: &dyn Clock, meter: &mut Meter, charge: WorkCharge) -> Result<(), ports::Error> {
    meter
        .charge(clock.sample()?.monotonic, charge)
        .map_err(|stop| {
            if stop == Stop::Deadline {
                ports::Error::Deadline
            } else {
                ports::Error::Capacity
            }
        })
}
impl<P: SubmissionAuthorization> StoreCoordinator<'_, '_, P> {
    /// Worker-only immediate submission with td-mail's implicit envelope. Caller
    /// prepays the complete response; this method neither sends SMTP nor applies
    /// on-success Email/set hooks. Stored Email bytes and flags remain intact.
    pub fn create_submission<C: Crypto>(
        &self,
        crypto: &C,
        entropy: &mut impl Entropy,
        spool: &IngressSpool<'_>,
        request: SubmissionRequest,
        meter: &mut Meter,
    ) -> Result<SubmissionCompletion, SubmissionError> {
        if request.maximum == 0
            || request.header_bytes == 0
            || request.header_bytes > crate::limits::MIB as u64
            || request.recipients == 0
            || request.recipients > 1000
            || meter.deadline() < request.deadline.into()
        {
            return Err(ports::Error::Invalid.into());
        }
        if request.maximum > spool.capacity().bytes_each {
            return Err(ports::Error::Capacity.into());
        }
        let mut value = scratch()?;
        let mut last = self.store.clock.sample()?.monotonic;
        let mut state = self.try_state()?;
        if state.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        if state.ledger.available_cells() < 2 {
            return Err(logical::Error::Full.into());
        }
        flags::charge(&self.store, request.deadline, &mut last, meter, 4)?;
        let guard = self.authorization.authorize_submission(
            request.account,
            request.device,
            request.identity,
            request.deadline,
        )?;
        let access = guard.access();
        validate_access(access, request.account, request.device)?;
        if guard.identity() != request.identity {
            return Err(ports::Error::Forbidden.into());
        }
        let mut identity_bytes = [0; 254];
        let identity_length = guard.email().len();
        identity_bytes
            .get_mut(..identity_length)
            .ok_or(ports::Error::Invalid)?
            .copy_from_slice(guard.email().as_bytes());
        let identity_email = std::str::from_utf8(
            identity_bytes
                .get(..identity_length)
                .ok_or(ports::Error::Invalid)?,
        )
        .map_err(|_| ports::Error::Invalid)?;
        let mut view = self.store.view(request.account, request.deadline)?;
        let (original, thread) = source(&mut view, request, &mut value)?;
        let source = Source {
            epoch: view.identity().epoch,
            blob: original,
            thread,
            access,
        };
        let mut bytes = [0; 16];
        entropy.fill(&mut bytes).map_err(ports::Error::from)?;
        let blob = BlobId::from_bytes(bytes);
        entropy.fill(&mut bytes).map_err(ports::Error::from)?;
        let submission = SubmissionId::from_bytes(bytes);
        self.submission_ids(
            &mut view, request, blob, submission, &mut value, &mut last, meter,
        )?;
        drop(guard);
        let time = check_time(&self.store, request.deadline, &mut last)?;
        let lease = state.ledger.reserve(
            &[[
                Charge {
                    kind: Kind::BodyBytes,
                    amount: request.maximum,
                },
                Charge {
                    kind: Kind::BlobCount,
                    amount: 1,
                },
                Charge {
                    kind: Kind::QueueBytes,
                    amount: request.maximum,
                },
                Charge {
                    kind: Kind::QueueSubmissions,
                    amount: 1,
                },
            ]],
            request.deadline,
            time.monotonic,
        )?;
        drop(state);
        let mut reservation = Reservation {
            coordinator: self,
            lease: Some(lease),
        };
        let clock = ComposeClock {
            deadline: request.deadline,
            clock: self.store.clock.as_ref(),
            last: std::sync::atomic::AtomicU64::new(last.0),
        };
        let mut verified = view.open_blob_input(crypto, original, request.maximum)?;
        let mut buffer = [0; 4096];
        while verified.position() < verified.len() {
            let amount = (verified.len() - verified.position()).min(buffer.len() as u64) as usize;
            pay(
                &clock,
                meter,
                WorkCharge {
                    io_bytes: amount as u64,
                    records: 1,
                    ..WorkCharge::default()
                },
            )?;
            let count = verified.read(buffer.get_mut(..amount).ok_or(ports::Error::Invalid)?)?;
            if count == 0 || count > amount {
                return Err(ports::Error::Corrupt.into());
            }
        }
        let mut pin = verified.finish()?;
        clock.sample()?;
        let length = usize::try_from(pin.len().min(request.header_bytes + 2))
            .map_err(|_| ports::Error::Capacity)?;
        let mut headers = Vec::new();
        headers
            .try_reserve_exact(length)
            .map_err(|_| ports::Error::Capacity)?;
        headers.resize(length, 0);
        let mut cursor = root_headers::Cursor::new(
            &mut pin,
            request.header_bytes,
            &mut headers,
            meter,
            clock.sample()?.monotonic,
        )
        .map_err(transmitted_copy::header)?;
        while cursor
            .poll(clock.sample()?.monotonic)
            .map_err(transmitted_copy::header)?
            != root_headers::Status::Complete
        {}
        let retained = cursor
            .finish(clock.sample()?.monotonic)
            .map_err(transmitted_copy::header)?;
        let end = usize::try_from(retained.end.body_start).map_err(|_| ports::Error::Invalid)?;
        let envelope = submission_envelope::derive(
            retained.bytes.get(..end).ok_or(ports::Error::Invalid)?,
            identity_email,
            request.recipients,
            request.deadline,
            &clock,
            meter,
        )?;
        pin.check_deadline()?;
        let mut writer = spool
            .begin(crypto, request.account, blob, request.deadline)
            .map_err(|error| {
                if error == ports::Error::Quota {
                    ports::Error::Busy
                } else {
                    error
                }
            })?;
        let size = transmitted_copy::write_retained(
            &mut pin,
            retained,
            transmitted_copy::Limits {
                header_bytes: request.header_bytes,
                message_bytes: request.maximum,
                deadline: request.deadline,
            },
            &clock,
            meter,
            |bytes| writer.write(bytes),
        )?;
        drop(pin);
        drop(view);
        let mut input = writer.finish()?;
        clock.sample()?;
        last = Tick(clock.last.load(std::sync::atomic::Ordering::Relaxed));
        if input.len() != size || input.id() != blob || input.account() != request.account {
            return Err(ports::Error::Invalid.into());
        }
        let result = {
            let mut state = self.try_state()?;
            self.publish_submission(
                crypto,
                request,
                source,
                identity_email,
                &envelope,
                submission,
                &mut input,
                &mut value,
                meter,
                &mut last,
                &mut state,
                &mut reservation,
            )?
        };
        Ok(SubmissionCompletion {
            cleanup_error: input.discard().err(),
            ..result
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn submission_ids(
        &self,
        view: &mut IndexReadView<'_, '_>,
        request: SubmissionRequest,
        blob: BlobId,
        submission: SubmissionId,
        value: &mut [u8],
        last: &mut Tick,
        meter: &mut Meter,
    ) -> Result<(), ports::Error> {
        flags::charge(&self.store, request.deadline, last, meter, 2)?;
        if view.get(Key::Blob(blob), value)?.is_some()
            || view.get(Key::Submission(submission), value)?.is_some()
            || self.used_identifier(
                view,
                (ObjectType::EmailSubmission, submission.as_bytes()),
                request.deadline,
                last,
                meter,
            )?
        {
            return Err(ports::Error::Conflict);
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn publish_submission<C: Crypto>(
        &self,
        crypto: &C,
        request: SubmissionRequest,
        original: Source,
        identity_email: &str,
        envelope: &Envelope<'_>,
        submission: SubmissionId,
        input: &mut SpoolInput<'_, '_>,
        value: &mut [u8],
        meter: &mut Meter,
        last: &mut Tick,
        state: &mut AdmissionState<'_>,
        reservation: &mut Reservation<'_, '_, '_, P>,
    ) -> Result<SubmissionCompletion, SubmissionError> {
        if state.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        flags::charge(&self.store, request.deadline, last, meter, 4)?;
        let guard = self.authorization.authorize_submission(
            request.account,
            request.device,
            request.identity,
            request.deadline,
        )?;
        validate_access(guard.access(), request.account, request.device)?;
        if guard.access() != original.access
            || guard.identity() != request.identity
            || guard.email() != identity_email
        {
            return Err(ports::Error::Forbidden.into());
        }
        let mut view = self.store.view(request.account, request.deadline)?;
        let before = view.identity();
        if before.epoch != original.epoch
            || source(&mut view, request, value)? != (original.blob, original.thread)
        {
            return Err(ports::Error::Conflict.into());
        }
        let blob = input.id();
        self.submission_ids(&mut view, request, blob, submission, value, last, meter)?;
        drop(view);
        let time = check_time(&self.store, request.deadline, last)?;
        let expires = time
            .utc_ms
            .checked_add(5 * 24 * 60 * 60 * 1000)
            .ok_or(ports::Error::Invalid)?;
        let recipients =
            u32::try_from(envelope.recipients().len()).map_err(|_| ports::Error::Capacity)?;
        let mut blob_row = [0; 48];
        let blob_length = Row::Blob(input.row(time.utc_ms)).encode(&mut blob_row)?;
        let length = Row::Submission(SubmissionRow {
            email: request.email,
            thread: original.thread,
            identity: request.identity,
            transmitted_blob: blob,
            reverse_path: envelope.mail_from(),
            send_at: time.utc_ms,
            expires_at: expires,
            recipient_count: recipients,
            completed_at: None,
            notification: NotificationState::None,
            notification_email: None,
        })
        .encode(value)?;
        // Every initial recipient fits this fixed row bound (254-byte address,
        // no replies or diagnostic); encoding still checks the actual extent.
        let capacity = (recipients as usize)
            .checked_mul(512)
            .ok_or(ports::Error::Capacity)?;
        let mut recipient_bytes = Vec::new();
        recipient_bytes
            .try_reserve_exact(capacity)
            .map_err(|_| ports::Error::Capacity)?;
        let mut extents = Vec::new();
        extents
            .try_reserve_exact(recipients as usize)
            .map_err(|_| ports::Error::Capacity)?;
        for (ordinal, address) in envelope.recipients().enumerate() {
            flags::charge(&self.store, request.deadline, last, meter, 1)?;
            let mut row = [0; 512];
            let count = Row::Recipient(RecipientRow {
                address,
                state: RecipientState::Queued,
                uncertain: false,
                attempt: None,
                attempt_count: 0,
                last_attempt_at: None,
                phase: AttemptPhase::None,
                next_attempt_at: Some(time.utc_ms),
                rcpt_reply: None,
                data_reply: None,
                reason: FailureReason::None,
                diagnostic: "",
            })
            .encode(&mut row)?;
            let mut key = [0; 20];
            let ordinal = u32::try_from(ordinal).map_err(|_| ports::Error::Capacity)?;
            let key_length = Key::Recipient(submission, ordinal).encode(&mut key)?;
            let start = recipient_bytes.len();
            recipient_bytes.extend_from_slice(row.get(..count).ok_or(ports::Error::Invalid)?);
            extents.push((key, key_length, start..recipient_bytes.len()));
        }
        let mut operations = Vec::new();
        operations
            .try_reserve_exact(extents.len() + 3)
            .map_err(|_| ports::Error::Capacity)?;
        operations.push(Operation::put(
            Table::Blobs,
            blob.as_bytes(),
            blob_row.get(..blob_length).ok_or(ports::Error::Invalid)?,
        )?);
        operations.push(Operation::put(
            Table::Submissions,
            submission.as_bytes(),
            value.get(..length).ok_or(ports::Error::Invalid)?,
        )?);
        for (key, length, range) in &extents {
            operations.push(Operation::put(
                Table::Recipients,
                key.get(..*length).ok_or(ports::Error::Invalid)?,
                recipient_bytes
                    .get(range.clone())
                    .ok_or(ports::Error::Invalid)?,
            )?);
        }
        operations.push(Operation::change(
            ObjectType::EmailSubmission,
            ChangeAction::Created,
            submission.as_bytes(),
        ));
        let mut encoded = 0usize;
        for operation in &operations {
            flags::charge(&self.store, request.deadline, last, meter, 1)?;
            encoded = encoded
                .checked_add(operation.encoded_len()?)
                .ok_or(ports::Error::Capacity)?;
        }
        if encoded > format::MAX_TRANSACTION_BYTES
            || operations.len() > format::MAX_TRANSACTION_OPERATIONS
        {
            return Err(ports::Error::Capacity.into());
        }
        let time_now = check_time(&self.store, request.deadline, last)?;
        meter
            .charge(
                time_now.monotonic,
                WorkCharge {
                    io_bytes: input.len(),
                    records: input.len().div_ceil(16 * 1024),
                    ..WorkCharge::default()
                },
            )
            .map_err(|stop| {
                if stop == Stop::Deadline {
                    ports::Error::Deadline
                } else {
                    ports::Error::Capacity
                }
            })?;
        let size = input.len();
        let planned = [size, 1, size, 1];
        let lease = reservation.lease.ok_or(ports::Error::Invalid)?;
        let mut effects = begin_publication(
            &mut state.ledger,
            &mut state.stopped,
            &mut state.recoverable_files,
            lease,
            planned,
            request.deadline,
            time_now.monotonic,
        )?;
        let completion = self.store.commit_with_files(
            crypto,
            CommitRequest {
                account: request.account,
                epoch: original.epoch,
                expected: before.committed_sequence,
                utc_ms: time.utc_ms,
                deadline: request.deadline,
            },
            &operations,
            &mut [BlobSource {
                id: blob,
                source: input,
            }],
        );
        drop(guard);
        let outcome = completion.outcome();
        let recoverable = settle_publication(
            &mut state.ledger,
            &mut state.stopped,
            &mut effects,
            planned,
            &completion,
        );
        let canceled = state.ledger.cancel(lease).is_ok();
        if !canceled {
            state.stopped = true;
        }
        state.recoverable_files = recoverable && canceled;
        reservation.lease = None;
        Ok(SubmissionCompletion {
            before,
            submission,
            blob,
            size,
            recipients,
            outcome,
            admission_stopped: state.stopped,
            cleanup_error: None,
        })
    }
}

#[cfg(test)]
#[path = "submission_tests.rs"]
mod tests;
