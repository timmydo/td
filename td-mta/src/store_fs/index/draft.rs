//! Device-authorized plain-text draft publication for td-mail.
use super::*;
use crate::{
    admission::work::{Charge as WorkCharge, Meter, Stop},
    compose_text::{self, Message},
    format::row::{EmailOrigin, EmailRow},
    ids::{EmailId, ThreadId},
};

/// Separate creation authority; a returned guard excludes revocation through
/// COMMIT. Acquisition obeys the deadline and never calls the coordinator.
pub trait DraftAuthorization {
    type Guard<'a>: UploadGuard
    where
        Self: 'a;
    fn authorize_draft(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, ports::Error>;
}
#[derive(Clone, Copy, Debug)]
pub struct DraftRequest {
    pub account: AccountId,
    pub device: DeviceId,
    pub mailbox: MailboxId,
    pub expected: Option<(StoreEpoch, Sequence)>,
    pub maximum: u64,
    /// May shorten, never extend, the original enclosing work deadline.
    pub deadline: Deadline,
}
#[derive(Debug)]
#[must_use = "preserve known publication even if accounting or cleanup fails"]
pub struct DraftCompletion {
    pub before: ViewIdentity,
    pub email: EmailId,
    pub blob: BlobId,
    pub thread: ThreadId,
    pub size: u64,
    pub outcome: Result<Sequence, CommitError>,
    pub admission_stopped: bool,
    pub cleanup_error: Option<ports::Error>,
}
// Bridge the composer's private time cursor back into publication checks.
pub(super) struct ComposeClock<'a> {
    pub(super) deadline: Deadline,
    pub(super) clock: &'a dyn Clock,
    pub(super) last: std::sync::atomic::AtomicU64,
}
impl Clock for ComposeClock<'_> {
    fn sample(&self) -> Result<Time, ports::Error> {
        use std::sync::atomic::Ordering;
        let time = self.clock.sample()?;
        if time.monotonic.0 < self.last.fetch_max(time.monotonic.0, Ordering::Relaxed) {
            return Err(ports::Error::Invalid);
        }
        if self.deadline.expired(time.monotonic) {
            return Err(ports::Error::Deadline);
        }
        Ok(time)
    }
}
pub(super) struct Reservation<'c, 'r, 'a, P> {
    pub(super) coordinator: &'c StoreCoordinator<'r, 'a, P>,
    pub(super) lease: Option<LeaseId>,
}
impl<P> Drop for Reservation<'_, '_, '_, P> {
    fn drop(&mut self) {
        let _ = self.coordinator.cancel_job(self.lease.take());
    }
}
pub(super) fn scratch() -> Result<Vec<u8>, ports::Error> {
    let mut value = Vec::new();
    value
        .try_reserve_exact(format::MAX_VALUE_BYTES)
        .map_err(|_| ports::Error::Capacity)?;
    value.resize(format::MAX_VALUE_BYTES, 0);
    Ok(value)
}
fn mailbox(
    view: &mut IndexReadView<'_, '_>,
    request: DraftRequest,
    value: &mut [u8],
) -> Result<(), ports::Error> {
    let before = view.identity();
    if request
        .expected
        .is_some_and(|expected| expected != (before.epoch, before.committed_sequence))
    {
        return Err(ports::Error::Conflict);
    }
    if !matches!(
        view.get(Key::Mailbox(request.mailbox), value)?,
        Some((Row::Mailbox(_), _))
    ) {
        return Err(ports::Error::NotFound);
    }
    Ok(())
}
impl<P: DraftAuthorization> StoreCoordinator<'_, '_, P> {
    /// Worker-side, one draft per call. The caller reserves its complete response
    /// before entry. No submission responsibility or sending identity is granted.
    pub fn create_text_draft<C: Crypto>(
        &self,
        crypto: &C,
        entropy: &mut impl Entropy,
        spool: &IngressSpool<'_>,
        request: DraftRequest,
        message: &Message<'_>,
        meter: &mut Meter,
    ) -> Result<DraftCompletion, UploadError> {
        if meter.deadline() < request.deadline.into() || request.maximum == 0 {
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
        flags::charge(&self.store, request.deadline, &mut last, meter, 1)?;
        let guard = self.authorization.authorize_draft(
            request.account,
            request.device,
            request.deadline,
        )?;
        let access = guard.access();
        validate_access(access, request.account, request.device)?;
        let mut view = self.store.view(request.account, request.deadline)?;
        mailbox(&mut view, request, &mut value)?;
        let epoch = view.identity().epoch;
        let mut bytes = [0; 16];
        entropy.fill(&mut bytes).map_err(ports::Error::from)?;
        let blob = BlobId::from_bytes(bytes);
        if view.get(Key::Blob(blob), &mut value)?.is_some() {
            return Err(ports::Error::Conflict.into());
        }
        drop(view);
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
                Charge::ZERO,
                Charge::ZERO,
            ]],
            request.deadline,
            time.monotonic,
        )?;
        drop(state);
        let mut reservation = Reservation {
            coordinator: self,
            lease: Some(lease),
        };
        let mut writer = spool
            .begin(crypto, request.account, blob, request.deadline)
            .map_err(|error| {
                // Occupied staging slots are transient pressure, not a draft
                // size refusal. Preserve Quota for the composer's size bound.
                if error == ports::Error::Quota {
                    ports::Error::Busy
                } else {
                    error
                }
            })?;
        let clock = ComposeClock {
            deadline: request.deadline,
            clock: self.store.clock.as_ref(),
            last: std::sync::atomic::AtomicU64::new(last.0),
        };
        let size = compose_text::compose(message, request.maximum, &clock, meter, |bytes| {
            writer.write(bytes)
        })?;
        last = Tick(clock.last.load(std::sync::atomic::Ordering::Relaxed));
        check_time(&self.store, request.deadline, &mut last)?;
        let mut input = writer.finish()?;
        check_time(&self.store, request.deadline, &mut last)?;
        if input.len() != size || input.id() != blob || input.account() != request.account {
            return Err(ports::Error::Invalid.into());
        }
        // No coordinator lock or authorization guard crosses composition.
        let result = {
            let mut state = self.try_state()?;
            self.publish_draft(
                crypto,
                entropy,
                request,
                message,
                meter,
                &mut last,
                epoch,
                access,
                blob,
                &mut input,
                &mut value,
                &mut state,
                &mut reservation,
            )?
        };
        Ok(DraftCompletion {
            cleanup_error: input.discard().err(),
            ..result
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_draft<C: Crypto>(
        &self,
        crypto: &C,
        entropy: &mut impl Entropy,
        request: DraftRequest,
        message: &Message<'_>,
        meter: &mut Meter,
        last: &mut Tick,
        epoch: StoreEpoch,
        access: Access,
        blob: BlobId,
        input: &mut SpoolInput<'_, '_>,
        value: &mut [u8],
        state: &mut AdmissionState<'_>,
        reservation: &mut Reservation<'_, '_, '_, P>,
    ) -> Result<DraftCompletion, UploadError> {
        if state.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        flags::charge(&self.store, request.deadline, last, meter, 4)?;
        let guard = self.authorization.authorize_draft(
            request.account,
            request.device,
            request.deadline,
        )?;
        validate_access(guard.access(), request.account, request.device)?;
        if guard.access() != access {
            return Err(ports::Error::Forbidden.into());
        }
        let mut view = self.store.view(request.account, request.deadline)?;
        let before = view.identity();
        if before.epoch != epoch {
            return Err(ports::Error::Conflict.into());
        }
        mailbox(&mut view, request, value)?;
        if view.get(Key::Blob(blob), value)?.is_some() {
            return Err(ports::Error::Conflict.into());
        }
        let mut bytes = [0; 16];
        entropy.fill(&mut bytes).map_err(ports::Error::from)?;
        let email = EmailId::from_bytes(bytes);
        if view.get(Key::Email(email), value)?.is_some()
            || self.used_identifier(
                &mut view,
                (ObjectType::Email, email.as_bytes()),
                request.deadline,
                last,
                meter,
            )?
        {
            return Err(ports::Error::Conflict.into());
        }
        let mut selected = None;
        for ids in [message.references, message.in_reply_to] {
            for id in ids.iter().rev().take(32) {
                flags::charge(&self.store, request.deadline, last, meter, 3)?;
                if let Some((_, thread)) = view.thread_anchor(id, value)? {
                    selected = Some(thread);
                    break;
                }
            }
            if selected.is_some() {
                break;
            }
        }
        if selected.is_none() {
            flags::charge(&self.store, request.deadline, last, meter, 3)?;
            selected = view
                .thread_anchor(message.message_id, value)?
                .map(|(_, thread)| thread);
        }
        let thread = if let Some(thread) = selected {
            thread
        } else {
            entropy.fill(&mut bytes).map_err(ports::Error::from)?;
            let thread = ThreadId::from_bytes(bytes);
            if view.get(Key::Thread(thread), value)?.is_some()
                || self.used_identifier(
                    &mut view,
                    (ObjectType::Thread, thread.as_bytes()),
                    request.deadline,
                    last,
                    meter,
                )?
            {
                return Err(ports::Error::Conflict.into());
            }
            thread
        };
        drop(view);
        flags::charge(&self.store, request.deadline, last, meter, 10)?;
        let time = check_time(&self.store, request.deadline, last)?;
        let mut blob_row = [0; 48];
        let blob_length = Row::Blob(input.row(time.utc_ms)).encode(&mut blob_row)?;
        let email_length = Row::Email(EmailRow {
            blob,
            thread,
            received_at: time.utc_ms,
            origin: EmailOrigin::Jmap,
        })
        .encode(value)?;
        let mut member = [0; 32];
        let member_length = Key::Membership(email, request.mailbox).encode(&mut member)?;
        let mut anchor = [0; 1024];
        let anchor_length = Key::ThreadAnchor(message.message_id, email).encode(&mut anchor)?;
        let mut draft = [0; 32];
        let draft_length = Key::Keyword(email, "$draft").encode(&mut draft)?;
        let mut seen = [0; 32];
        let seen_length = Key::Keyword(email, "$seen").encode(&mut seen)?;
        let mut operations = Vec::new();
        operations
            .try_reserve_exact(10)
            .map_err(|_| ports::Error::Capacity)?;
        operations.push(Operation::put(
            Table::Blobs,
            blob.as_bytes(),
            blob_row.get(..blob_length).ok_or(ports::Error::Invalid)?,
        )?);
        if selected.is_none() {
            operations.push(Operation::put(Table::Threads, thread.as_bytes(), &[])?)
        }
        operations.push(Operation::put(
            Table::Emails,
            email.as_bytes(),
            value.get(..email_length).ok_or(ports::Error::Invalid)?,
        )?);
        for (table, key, length) in [
            (Table::Memberships, member.as_slice(), member_length),
            (Table::Keywords, draft.as_slice(), draft_length),
            (Table::Keywords, seen.as_slice(), seen_length),
            (Table::ThreadAnchors, anchor.as_slice(), anchor_length),
        ] {
            operations.push(Operation::put(
                table,
                key.get(..length).ok_or(ports::Error::Invalid)?,
                &[],
            )?)
        }
        operations.push(Operation::change(
            ObjectType::Email,
            ChangeAction::Created,
            email.as_bytes(),
        ));
        operations.push(Operation::change(
            ObjectType::Thread,
            if selected.is_some() {
                ChangeAction::Updated
            } else {
                ChangeAction::Created
            },
            thread.as_bytes(),
        ));
        operations.push(Operation::change(
            ObjectType::Mailbox,
            ChangeAction::Updated,
            request.mailbox.as_bytes(),
        ));
        // Native body copying reads the prepared extent once more.
        meter
            .charge(
                time.monotonic,
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
        let planned = [input.len(), 1, 0, 0];
        let lease = reservation.lease.ok_or(ports::Error::Invalid)?;
        let mut effects = begin_publication(
            &mut state.ledger,
            &mut state.stopped,
            &mut state.recoverable_files,
            lease,
            planned,
            request.deadline,
            time.monotonic,
        )?;
        let completion = self.store.commit_with_files(
            crypto,
            CommitRequest {
                account: request.account,
                epoch,
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
        Ok(DraftCompletion {
            before,
            email,
            blob,
            thread,
            size: planned[0],
            outcome,
            admission_stopped: state.stopped,
            cleanup_error: None,
        })
    }
}

impl<P> StoreCoordinator<'_, '_, P> {
    // Walk retained kind history once using its existing ordered index. Every
    // row is prepaid; exhaustion refuses rather than accepting an unchecked ID.
    pub(super) fn used_identifier(
        &self,
        view: &mut IndexReadView<'_, '_>,
        target: (ObjectType, &[u8; 16]),
        deadline: Deadline,
        last: &mut Tick,
        meter: &mut Meter,
    ) -> Result<bool, ports::Error> {
        let (kind, id) = target;
        let identity = view.identity();
        view.read_snapshot(|native| native.run(|db| {
            flags::charge(&self.store, deadline, last, meter, 1)?;
            let mut statement = db.prepare("SELECT object FROM changes INDEXED BY changes_kind WHERE account=?1 AND kind=?2 AND sequence>?3 AND sequence<=?4 ORDER BY sequence,operation").map_err(sql)?;
            let mut rows = statement.query(params![identity.account.as_bytes().as_slice(), kind.tag(), identity.history_floor.number().to_be_bytes().as_slice(), identity.committed_sequence.number().to_be_bytes().as_slice()]).map_err(sql)?;
            loop {
                flags::charge(&self.store, deadline, last, meter, 1)?;
                let Some(row) = rows.next().map_err(sql)? else { return Ok(false) };
                if fixed_blob::<16>(row, 0)? == *id { return Ok(true) }
            }
        }))
    }
}

#[cfg(test)]
#[path = "draft_tests.rs"]
mod tests;
