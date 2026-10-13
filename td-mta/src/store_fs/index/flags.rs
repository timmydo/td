//! Device-authorized read/star assignments and their history in one transaction.
use super::*;
use crate::{
    admission::work::{Charge as WorkCharge, Meter, Stop},
    ids::EmailId,
};

pub const MAX_FLAG_UPDATES: usize = 256;
const MEMBERSHIP_WINDOW: usize = 16;
const FLAG_KEY_BYTES: usize = 16 + "$flagged".len();
/// Separate permission from uploads and reads. A guard must exclude revocation
/// through COMMIT, obey the deadline, and never call back into the coordinator.
pub trait FlagAuthorization {
    type Guard<'a>: UploadGuard
    where
        Self: 'a;
    fn authorize_flags(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, ports::Error>;
}
#[derive(Clone, Copy, Debug)]
pub struct EmailFlagUpdate {
    pub email: EmailId,
    pub seen: Option<bool>,
    pub flagged: Option<bool>,
}
/// Passive inputs. Authorization and account state are checked under coordination.
#[derive(Clone, Copy, Debug)]
pub struct FlagRequest<'a> {
    pub account: AccountId,
    pub device: DeviceId,
    pub expected: Option<(StoreEpoch, Sequence)>,
    pub updates: &'a [EmailFlagUpdate],
    pub deadline: Deadline,
}
#[derive(Debug)]
#[must_use = "preserve the durable result even when admission has stopped"]
pub struct FlagCompletion {
    pub before: ViewIdentity,
    pub outcome: Result<Sequence, CommitError>,
    /// In request order; absent emails have no writes or history.
    pub found: Vec<bool>,
    pub admission_stopped: bool,
}
struct KeywordChange {
    key: [u8; FLAG_KEY_BYTES],
    length: usize,
    set: bool,
}
impl<P: FlagAuthorization> StoreCoordinator<'_, '_, P> {
    /// Caller reserves complete response capacity before invoking this mutation.
    /// The supplied meter belongs to the enclosing request and is never renewed.
    pub fn update_flags<C: Crypto>(
        &self,
        crypto: &C,
        request: FlagRequest<'_>,
        meter: &mut Meter,
    ) -> Result<FlagCompletion, UploadError> {
        if meter.deadline() < request.deadline.into() {
            return Err(ports::Error::Invalid.into());
        }
        if request.updates.len() > MAX_FLAG_UPDATES {
            return Err(ports::Error::Capacity.into());
        }
        for (index, update) in request.updates.iter().enumerate() {
            if request
                .updates
                .get(..index)
                .ok_or(ports::Error::Invalid)?
                .iter()
                .any(|prior| prior.email == update.email)
            {
                return Err(ports::Error::Invalid.into());
            }
        }
        let mut state_guard = self.try_state()?;
        let state = &mut *state_guard;
        if state.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        let mut last = self.store.clock.sample()?.monotonic;
        check_time(&self.store, request.deadline, &mut last)?;
        let guard = self.authorization.authorize_flags(
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
        let mut found = Vec::new();
        let mut keywords = Vec::new();
        let mut emails = Vec::new();
        let mut mailboxes = Vec::new();
        found
            .try_reserve_exact(request.updates.len())
            .map_err(|_| ports::Error::Capacity)?;
        keywords
            .try_reserve_exact(request.updates.len() * 2)
            .map_err(|_| ports::Error::Capacity)?;
        emails
            .try_reserve_exact(request.updates.len())
            .map_err(|_| ports::Error::Capacity)?;
        mailboxes
            .try_reserve_exact(MAX_OPERATIONS)
            .map_err(|_| ports::Error::Capacity)?;
        for update in request.updates {
            charge(&self.store, request.deadline, &mut last, meter, 1)?;
            let exists = view.get(Key::Email(update.email), &mut scratch)?.is_some();
            found.push(exists);
            if !exists {
                continue;
            }
            let mut changed = false;
            let mut seen_changed = false;
            for (name, desired) in [("$seen", update.seen), ("$flagged", update.flagged)] {
                let Some(desired) = desired else {
                    continue;
                };
                charge(&self.store, request.deadline, &mut last, meter, 1)?;
                let key = Key::Keyword(update.email, name);
                if view.get(key, &mut scratch)?.is_some() == desired {
                    continue;
                }
                let mut bytes = [0; FLAG_KEY_BYTES];
                let length = key.encode(&mut bytes)?;
                keywords.push(KeywordChange {
                    key: bytes,
                    length,
                    set: desired,
                });
                changed = true;
                seen_changed |= name == "$seen";
            }
            if changed {
                emails.push(update.email);
            }
            if seen_changed {
                let mut after = None;
                loop {
                    // Reserve work for the native window and its sentinel before reading.
                    charge(
                        &self.store,
                        request.deadline,
                        &mut last,
                        meter,
                        (MEMBERSHIP_WINDOW + 1) as u64,
                    )?;
                    let window = view.email_mailboxes(update.email, after, MEMBERSHIP_WINDOW)?;
                    for mailbox in &window.values {
                        if let Err(position) = mailboxes.binary_search(mailbox) {
                            if keywords.len() + emails.len() + mailboxes.len() >= MAX_OPERATIONS {
                                return Err(ports::Error::Capacity.into());
                            }
                            mailboxes.insert(position, *mailbox);
                        }
                    }
                    if !window.has_more {
                        break;
                    }
                    after = Some(*window.values.last().ok_or(ports::Error::Corrupt)?);
                }
            }
        }
        drop(view);
        let count = keywords.len() + emails.len() + mailboxes.len();
        if count > MAX_OPERATIONS {
            return Err(ports::Error::Capacity.into());
        }
        charge(
            &self.store,
            request.deadline,
            &mut last,
            meter,
            count as u64,
        )?;
        let mut operations = Vec::new();
        operations
            .try_reserve_exact(count)
            .map_err(|_| ports::Error::Capacity)?;
        for keyword in &keywords {
            let key = keyword
                .key
                .get(..keyword.length)
                .ok_or(ports::Error::Invalid)?;
            operations.push(if keyword.set {
                Operation::put(Table::Keywords, key, &[])?
            } else {
                Operation::delete(Table::Keywords, key)?
            });
        }
        for email in &emails {
            operations.push(Operation::change(
                ObjectType::Email,
                ChangeAction::Updated,
                email.as_bytes(),
            ));
        }
        for mailbox in &mailboxes {
            operations.push(Operation::change(
                ObjectType::Mailbox,
                ChangeAction::Updated,
                mailbox.as_bytes(),
            ));
        }
        if operations.is_empty() {
            return Ok(FlagCompletion {
                before,
                outcome: Ok(before.committed_sequence),
                found,
                admission_stopped: false,
            });
        }
        let time = check_time(&self.store, request.deadline, &mut last)?;
        let (lease, mut ticket) = reserve_physical(
            &mut state.ledger,
            &mut state.stopped,
            &mut state.recoverable_files,
            request.deadline,
            time.monotonic,
        )?;
        let completion = self.store.commit_with_files(
            crypto,
            CommitRequest {
                account: request.account,
                epoch: before.epoch,
                expected: before.committed_sequence,
                utc_ms: time.utc_ms,
                deadline: request.deadline,
            },
            &operations,
            &mut [],
        );
        drop(guard);
        let accounted = settle_physical(&mut state.ledger, &mut ticket, completion.files());
        let canceled = state.ledger.cancel(lease).is_ok();
        if !accounted || !canceled || completion.writer_stopped() {
            state.stopped = true;
            state.recoverable_files = false;
        } else if matches!(completion.files(), CommitFileUsage::Unavailable(_)) {
            state.stopped = true;
            state.recoverable_files = true;
        }
        Ok(FlagCompletion {
            before,
            outcome: completion.outcome(),
            found,
            admission_stopped: state.stopped,
        })
    }
}
fn charge(
    store: &IndexStore<'_>,
    deadline: Deadline,
    last: &mut Tick,
    meter: &mut Meter,
    records: u64,
) -> Result<(), ports::Error> {
    let time = check_time(store, deadline, last)?;
    meter
        .charge(
            time.monotonic,
            WorkCharge {
                records,
                ..WorkCharge::default()
            },
        )
        .map_err(|error| match error {
            Stop::Deadline => ports::Error::Deadline,
            _ => ports::Error::Capacity,
        })
}
