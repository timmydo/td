//! Device-authorized deletion and last-owner body retirement in one transaction.
use super::*;
use crate::{admission::work::Meter, ids::EmailId};

pub const MAX_EMAIL_DESTROYS: usize = 256;
#[derive(Clone, Copy, Debug)]
pub struct EmailDestroyRequest<'a> {
    pub account: AccountId,
    pub device: DeviceId,
    pub expected: Option<(StoreEpoch, Sequence)>,
    pub emails: &'a [EmailId],
    pub deadline: Deadline,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmailDestroyResult {
    Destroyed,
    NotFound,
}
#[derive(Debug)]
#[must_use = "only a known commit makes the planned deletions durable"]
pub struct EmailDestroyCompletion {
    pub before_epoch: StoreEpoch,
    pub before_sequence: Sequence,
    pub outcome: Result<Sequence, CommitError>,
    /// Planned per-object outcomes; durable only when outcome is Ok.
    pub results: Vec<EmailDestroyResult>,
    pub admission_stopped: bool,
}
struct Deletion {
    table: Table,
    key: Vec<u8>,
}
struct Plan {
    bytes: usize,
    deletes: Vec<Deletion>,
    changes: Vec<(ObjectType, ChangeAction, [u8; 16])>,
}
impl Plan {
    fn new() -> Result<Self, ports::Error> {
        let mut deletes = Vec::new();
        let mut changes = Vec::new();
        deletes
            .try_reserve_exact(MAX_OPERATIONS)
            .map_err(|_| ports::Error::Capacity)?;
        changes
            .try_reserve_exact(MAX_OPERATIONS)
            .map_err(|_| ports::Error::Capacity)?;
        Ok(Self {
            bytes: 128,
            deletes,
            changes,
        })
    }
    fn room(&self) -> Result<(), ports::Error> {
        if self.deletes.len() + self.changes.len() >= MAX_OPERATIONS {
            return Err(ports::Error::Capacity);
        }
        Ok(())
    }
    fn delete(&mut self, key: Key<'_>) -> Result<(), ports::Error> {
        self.room()?;
        let mut bytes = [0; format::MAX_KEY_BYTES];
        let length = key.encode(&mut bytes).map_err(|_| ports::Error::Invalid)?;
        let value = bytes.get(..length).ok_or(ports::Error::Invalid)?;
        let next = self
            .bytes
            .checked_add(length + format::OPERATION_HEADER_BYTES)
            .ok_or(ports::Error::Capacity)?;
        if next > MAX_TRANSACTION_BYTES {
            return Err(ports::Error::Capacity);
        }
        self.bytes = next;
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(length)
            .map_err(|_| ports::Error::Capacity)?;
        owned.extend_from_slice(value);
        self.deletes.push(Deletion {
            table: key.table(),
            key: owned,
        });
        Ok(())
    }
    fn change(
        &mut self,
        kind: ObjectType,
        action: ChangeAction,
        id: [u8; 16],
    ) -> Result<(), ports::Error> {
        if self.changes.contains(&(kind, action, id)) {
            return Ok(());
        }
        self.room()?;
        let next = self
            .bytes
            .checked_add(16 + format::OPERATION_HEADER_BYTES)
            .ok_or(ports::Error::Capacity)?;
        if next > MAX_TRANSACTION_BYTES {
            return Err(ports::Error::Capacity);
        }
        self.bytes = next;
        self.changes.push((kind, action, id));
        Ok(())
    }
}
impl<P: EmailUpdateAuthorization> StoreCoordinator<'_, '_, P> {
    /// Reserve the entire response first. The original meter is not renewed.
    pub fn destroy_emails<C: Crypto>(
        &self,
        crypto: &C,
        request: EmailDestroyRequest<'_>,
        meter: &mut Meter,
    ) -> Result<EmailDestroyCompletion, UploadError> {
        if meter.deadline() < request.deadline.into() {
            return Err(ports::Error::Invalid.into());
        }
        if request.emails.len() > MAX_EMAIL_DESTROYS {
            return Err(ports::Error::Capacity.into());
        }
        let mut state_guard = self.try_state()?;
        let state = &mut *state_guard;
        if state.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        let mut last = self.store.clock.sample()?.monotonic;
        check_time(&self.store, request.deadline, &mut last)?;
        let guard = self.authorization.authorize_email_update(
            request.account,
            request.device,
            request.deadline,
        )?;
        validate_access(guard.access(), request.account, request.device)?;
        let mut view = self.store.view(request.account, request.deadline)?;
        let before = view.identity();
        if request
            .expected
            .is_some_and(|expected| expected != (before.epoch, before.committed_sequence))
        {
            return Err(ports::Error::Conflict.into());
        }
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(format::MAX_VALUE_BYTES)
            .map_err(|_| ports::Error::Capacity)?;
        scratch.resize(format::MAX_VALUE_BYTES, 0);
        let mut results = Vec::new();
        let mut blobs = Vec::new();
        let mut threads = Vec::new();
        results
            .try_reserve_exact(request.emails.len())
            .map_err(|_| ports::Error::Capacity)?;
        blobs
            .try_reserve_exact(request.emails.len())
            .map_err(|_| ports::Error::Capacity)?;
        threads
            .try_reserve_exact(request.emails.len())
            .map_err(|_| ports::Error::Capacity)?;
        let mut deleted = Vec::new();
        deleted
            .try_reserve_exact(request.emails.len())
            .map_err(|_| ports::Error::Capacity)?;
        flags::charge(
            &self.store,
            request.deadline,
            &mut last,
            meter,
            (request.emails.len() * request.emails.len() + request.emails.len()) as u64,
        )?;
        deleted.extend_from_slice(request.emails);
        deleted.sort_unstable();
        if deleted.windows(2).any(|pair| pair.first() == pair.last()) {
            return Err(ports::Error::Invalid.into());
        }
        let mut plan = Plan::new()?;
        for email in request.emails {
            flags::charge(
                &self.store,
                request.deadline,
                &mut last,
                meter,
                plan.changes.len() as u64 + request.emails.len() as u64 * 2 + 2,
            )?;
            let Some(metadata) = view.email_metadata(*email, &mut scratch)? else {
                results.push(EmailDestroyResult::NotFound);
                continue;
            };
            results.push(EmailDestroyResult::Destroyed);
            if !blobs.contains(&(metadata.blob, metadata.size)) {
                blobs.push((metadata.blob, metadata.size));
            }
            if !threads.contains(&metadata.thread) {
                threads.push(metadata.thread);
            }
            plan.delete(Key::Email(*email))?;
            plan.change(
                ObjectType::Email,
                ChangeAction::Destroyed,
                *email.as_bytes(),
            )?;
            let mut after = None;
            loop {
                flags::charge(&self.store, request.deadline, &mut last, meter, 17)?;
                let window = view.email_mailboxes(*email, after, 16)?;
                for mailbox in &window.values {
                    flags::charge(
                        &self.store,
                        request.deadline,
                        &mut last,
                        meter,
                        plan.changes.len() as u64 + 1,
                    )?;
                    plan.delete(Key::Membership(*email, *mailbox))?;
                    plan.change(
                        ObjectType::Mailbox,
                        ChangeAction::Updated,
                        *mailbox.as_bytes(),
                    )?;
                }
                if !window.has_more {
                    break;
                }
                after = Some(*window.values.last().ok_or(ports::Error::Corrupt)?);
            }
            let mut after = None;
            loop {
                flags::charge(&self.store, request.deadline, &mut last, meter, 17)?;
                let mut window = view.email_keywords(*email, after.as_deref(), 16)?;
                for keyword in &window.values {
                    plan.delete(Key::Keyword(*email, keyword))?;
                }
                if !window.has_more {
                    break;
                }
                after = window.values.pop();
                if after.is_none() {
                    return Err(ports::Error::Corrupt.into());
                }
            }
            flags::charge(&self.store, request.deadline, &mut last, meter, 2)?;
            if let Some(anchor) = deletion_anchor(&mut view, *email)? {
                plan.delete(Key::ThreadAnchor(&anchor, *email))?;
            }
        }
        for thread in threads {
            flags::charge(
                &self.store,
                request.deadline,
                &mut last,
                meter,
                ((MAX_EMAIL_DESTROYS + 1) * 10 + plan.changes.len()) as u64,
            )?;
            let survives = surviving_email(&mut view, THREAD_OWNERS, thread.as_bytes(), &deleted)?;
            if !survives {
                plan.delete(Key::Thread(thread))?;
            }
            plan.change(
                ObjectType::Thread,
                if survives {
                    ChangeAction::Updated
                } else {
                    ChangeAction::Destroyed
                },
                *thread.as_bytes(),
            )?;
        }
        let (mut released_bytes, mut released_count) = (0u64, 0u64);
        for (blob, size) in blobs {
            flags::charge(
                &self.store,
                request.deadline,
                &mut last,
                meter,
                ((MAX_EMAIL_DESTROYS + 1) * 10 + 2) as u64,
            )?;
            if surviving_email(&mut view, BLOB_OWNERS, blob.as_bytes(), &deleted)?
                || other_blob_owner(&mut view, blob)?
            {
                continue;
            }
            plan.delete(Key::Blob(blob))?;
            released_bytes = released_bytes
                .checked_add(size)
                .ok_or(ports::Error::Capacity)?;
            released_count = released_count
                .checked_add(1)
                .ok_or(ports::Error::Capacity)?;
        }
        drop(view);
        flags::charge(
            &self.store,
            request.deadline,
            &mut last,
            meter,
            (plan.deletes.len() + plan.changes.len()) as u64,
        )?;
        let mut operations = Vec::new();
        operations
            .try_reserve_exact(plan.deletes.len() + plan.changes.len())
            .map_err(|_| ports::Error::Capacity)?;
        for entry in &plan.deletes {
            operations.push(Operation::delete(entry.table, &entry.key)?);
        }
        for (kind, action, id) in &plan.changes {
            operations.push(Operation::change(*kind, *action, id));
        }
        state
            .ledger
            .check_deleted_bodies(released_bytes, released_count)?;
        let outcome = self.commit_metadata(
            crypto,
            state,
            before,
            request.deadline,
            &mut last,
            &operations,
        )?;
        let settled = match outcome {
            Ok(_) => state
                .ledger
                .complete_deleted_bodies(released_bytes, released_count)
                .is_ok(),
            Err(CommitError::Rejected(_)) => true,
            Err(CommitError::Indeterminate(_)) => false,
        };
        if !settled {
            state.stopped = true;
            state.recoverable_files = false;
        }
        drop(guard);
        Ok(EmailDestroyCompletion {
            before_epoch: before.epoch,
            before_sequence: before.committed_sequence,
            outcome,
            results,
            admission_stopped: state.stopped,
        })
    }
}
// At most 256 selected IDs: any 257 owners include a survivor.
// Binary search takes at most nine comparisons per inspected owner.
const THREAD_OWNERS: &str = "SELECT id,changed FROM emails INDEXED BY emails_thread WHERE account=?1 AND thread_id=?2 LIMIT 257";
const BLOB_OWNERS: &str = "SELECT id,changed FROM emails INDEXED BY emails_blob WHERE account=?1 AND blob_id=?2 LIMIT 257";
fn surviving_email(
    view: &mut IndexReadView<'_, '_>,
    query: &str,
    id: &[u8; 16],
    deleted: &[EmailId],
) -> Result<bool, ports::Error> {
    let identity = view.identity();
    view.read_snapshot(|native| {
        native.run(|db| {
            let mut statement = db.prepare(query).map_err(sql)?;
            let mut rows = statement
                .query(params![
                    identity.account.as_bytes().as_slice(),
                    id.as_slice()
                ])
                .map_err(sql)?;
            while let Some(row) = rows.next().map_err(sql)? {
                let email = EmailId::from_bytes(fixed_blob::<16>(row, 0)?);
                if sequence(&fixed_blob::<8>(row, 1)?)? > identity.committed_sequence {
                    return Err(ports::Error::Corrupt);
                }
                if deleted.binary_search(&email).is_err() {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    })
}
fn other_blob_owner(view: &mut IndexReadView<'_, '_>, blob: BlobId) -> Result<bool, ports::Error> {
    let identity = view.identity();
    view.read_snapshot(|native| native.run(|db| db.query_row(
        "SELECT EXISTS(SELECT 1 FROM submissions INDEXED BY submissions_blob WHERE account=?1 AND transmitted_blob_id=?2) OR EXISTS(SELECT 1 FROM leases WHERE account=?1 AND blob_id=?2)",
        params![identity.account.as_bytes().as_slice(), blob.as_bytes().as_slice()], |row| row.get(0)).map_err(sql)))
}
fn deletion_anchor(
    view: &mut IndexReadView<'_, '_>,
    email: EmailId,
) -> Result<Option<String>, ports::Error> {
    let identity = view.identity();
    view.read_snapshot(|native| native.run(|db| {
        let mut statement = db.prepare("SELECT message_id,changed FROM thread_anchors INDEXED BY anchors_email WHERE account=?1 AND email_id=?2 LIMIT 2").map_err(sql)?;
        let mut rows = statement.query(params![identity.account.as_bytes().as_slice(), email.as_bytes().as_slice()]).map_err(sql)?;
        let Some(row) = rows.next().map_err(sql)? else { return Ok(None); };
        let ValueRef::Text(bytes) = row.get_ref(0).map_err(sql)? else { return Err(ports::Error::Corrupt); };
        let name = std::str::from_utf8(bytes).map_err(|_| ports::Error::Corrupt)?;
        Key::ThreadAnchor(name, email).validate_local().map_err(|_| ports::Error::Corrupt)?;
        if sequence(&fixed_blob::<8>(row, 1)?)? > identity.committed_sequence { return Err(ports::Error::Corrupt); }
        let mut owned = String::new();
        owned.try_reserve_exact(name.len()).map_err(|_| ports::Error::Capacity)?;
        owned.push_str(name);
        if rows.next().map_err(sql)?.is_some() { return Err(ports::Error::Corrupt); }
        Ok(Some(owned))
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn deletion_plan_caps_operations_and_encoded_bytes() {
        let mut plan = Plan::new().unwrap();
        for index in 0..MAX_OPERATIONS {
            let id = EmailId::from_bytes((index as u128).to_be_bytes());
            plan.delete(Key::Email(id)).unwrap();
        }
        assert_eq!(
            plan.delete(Key::Email(EmailId::from_bytes([0; 16]))),
            Err(ports::Error::Capacity)
        );
        let mut plan = Plan::new().unwrap();
        plan.bytes = MAX_TRANSACTION_BYTES - 1;
        assert_eq!(
            plan.delete(Key::Email(EmailId::from_bytes([0; 16]))),
            Err(ports::Error::Capacity)
        );
        assert!(plan.deletes.is_empty());
    }
}
