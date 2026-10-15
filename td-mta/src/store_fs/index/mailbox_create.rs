//! Device-authorized root-folder creation for td-mail.
use super::*;
use crate::admission::work::Meter;

pub const MAX_MAILBOX_CREATES: usize = 256;

pub trait MailboxAuthorization {
    type Guard<'a>: UploadGuard
    where
        Self: 'a;
    /// Bind the account/device and exclude revocation through COMMIT.
    /// Obey the deadline and never call back into the coordinator.
    fn authorize_mailboxes(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Self::Guard<'_>, ports::Error>;
}
#[derive(Clone, Copy)]
pub struct MailboxCreateRequest<'a> {
    pub account: AccountId,
    pub device: DeviceId,
    pub expected: Option<(StoreEpoch, Sequence)>,
    pub names: &'a [&'a str],
    pub deadline: Deadline,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MailboxCreateResult {
    Created(MailboxId),
    InvalidName,
    AlreadyExists,
}
#[derive(Debug)]
#[must_use = "preserve the durable result even when admission has stopped"]
pub struct MailboxCreateCompletion {
    pub before_epoch: StoreEpoch,
    pub before_sequence: Sequence,
    pub outcome: Result<Sequence, CommitError>,
    /// Planned results become durable only when outcome is Ok.
    pub results: Vec<MailboxCreateResult>,
    pub admission_stopped: bool,
}
enum Planned {
    Create,
    Refused(MailboxCreateResult),
}
impl<P: MailboxAuthorization> StoreCoordinator<'_, '_, P> {
    /// The caller reserves complete response capacity before entry.
    pub fn create_mailboxes<C: Crypto>(
        &self,
        crypto: &C,
        entropy: &mut impl Entropy,
        request: MailboxCreateRequest<'_>,
        meter: &mut Meter,
    ) -> Result<MailboxCreateCompletion, UploadError> {
        if request.names.len() > MAX_MAILBOX_CREATES {
            return Err(ports::Error::Capacity.into());
        }
        if meter.deadline() < request.deadline.into() {
            return Err(ports::Error::Invalid.into());
        }
        let mut state_guard = self.try_state()?;
        let state = &mut *state_guard;
        if state.stopped {
            return Err(ports::Error::WriterStopped.into());
        }
        let mut last = self.store.clock.sample()?.monotonic;
        check_time(&self.store, request.deadline, &mut last)?;
        let guard = self.authorization.authorize_mailboxes(
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
        let mut planned = Vec::new();
        planned
            .try_reserve_exact(request.names.len())
            .map_err(|_| ports::Error::Capacity)?;
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(format::MAX_VALUE_BYTES)
            .map_err(|_| ports::Error::Capacity)?;
        scratch.resize(format::MAX_VALUE_BYTES, 0);
        let mut normalization = td_mime::nfc::Scratch::new();
        for (index, name) in request.names.iter().enumerate() {
            flags::charge(&self.store, request.deadline, &mut last, meter, 1)?;
            let mut budget = td_mime::nfc::HeaderBudget::new();
            if !valid_name(
                &self.store,
                request.deadline,
                &mut last,
                meter,
                &mut budget,
                &mut normalization,
                name,
            )? {
                planned.push(Planned::Refused(MailboxCreateResult::InvalidName));
                continue;
            }
            flags::charge(
                &self.store,
                request.deadline,
                &mut last,
                meter,
                index as u64,
            )?;
            if request
                .names
                .get(..index)
                .ok_or(ports::Error::Invalid)?
                .iter()
                .zip(&planned)
                .any(|(prior, result)| prior == name && matches!(result, Planned::Create))
            {
                planned.push(Planned::Refused(MailboxCreateResult::AlreadyExists));
                continue;
            }
            planned.push(Planned::Create);
        }
        let mut after = None::<[u8; 16]>;
        let mut key = [0; 16];
        while planned
            .iter()
            .any(|result| matches!(result, Planned::Create))
        {
            flags::charge(
                &self.store,
                request.deadline,
                &mut last,
                meter,
                1 + 2 * request.names.len() as u64,
            )?;
            let Some(record) = view.next(
                Table::Mailboxes,
                after.as_ref().map(|key| key.as_slice()),
                &mut key,
                &mut scratch,
            )?
            else {
                break;
            };
            let (Key::Mailbox(id), Row::Mailbox(row)) = (record.key, record.row) else {
                return Err(ports::Error::Corrupt.into());
            };
            if row.parent.is_none() {
                for (name, result) in request.names.iter().zip(&mut planned) {
                    if *name == row.name && matches!(result, Planned::Create) {
                        *result = Planned::Refused(MailboxCreateResult::AlreadyExists);
                    }
                }
            }
            after = Some(*id.as_bytes());
        }
        let mut results = Vec::new();
        results
            .try_reserve_exact(planned.len())
            .map_err(|_| ports::Error::Capacity)?;
        for plan in planned {
            if let Planned::Refused(result) = plan {
                results.push(result);
                continue;
            }
            // Only new names consume entropy; retries do not need a fresh ID.
            let mut selected = None;
            for _ in 0..4 {
                flags::charge(
                    &self.store,
                    request.deadline,
                    &mut last,
                    meter,
                    1 + results.len() as u64,
                )?;
                let mut bytes = [0; 16];
                entropy.fill(&mut bytes).map_err(ports::Error::from)?;
                let id = MailboxId::from_bytes(bytes);
                if !results.contains(&MailboxCreateResult::Created(id))
                    && view.get(Key::Mailbox(id), &mut scratch)?.is_none()
                {
                    selected = Some(id);
                    break;
                }
            }
            results.push(MailboxCreateResult::Created(
                selected.ok_or(ports::Error::Capacity)?,
            ));
        }
        drop(view);
        let mut rows = Vec::new();
        rows.try_reserve_exact(results.len())
            .map_err(|_| ports::Error::Capacity)?;
        for (name, result) in request.names.iter().zip(&results) {
            if let MailboxCreateResult::Created(id) = result {
                let row = Row::Mailbox(crate::format::row::MailboxRow {
                    name,
                    parent: None,
                    role: None,
                    sort_order: 0,
                    subscribed: true,
                });
                let mut encoded = Vec::new();
                encoded
                    .try_reserve_exact(format::row::MAX_MAILBOX_NAME + 64)
                    .map_err(|_| ports::Error::Capacity)?;
                encoded.resize(format::row::MAX_MAILBOX_NAME + 64, 0);
                let length = row.encode(&mut encoded)?;
                encoded.truncate(length);
                rows.push((*id, encoded));
            }
        }
        flags::charge(
            &self.store,
            request.deadline,
            &mut last,
            meter,
            (rows.len() * 2) as u64,
        )?;
        let mut operations = Vec::new();
        operations
            .try_reserve_exact(rows.len() * 2)
            .map_err(|_| ports::Error::Capacity)?;
        for (id, row) in &rows {
            operations.push(Operation::put(Table::Mailboxes, id.as_bytes(), row)?);
            operations.push(Operation::change(
                ObjectType::Mailbox,
                ChangeAction::Created,
                id.as_bytes(),
            ));
        }
        let outcome = self.commit_metadata(
            crypto,
            state,
            before,
            request.deadline,
            &mut last,
            &operations,
        )?;
        drop(guard);
        Ok(MailboxCreateCompletion {
            before_epoch: before.epoch,
            before_sequence: before.committed_sequence,
            outcome,
            results,
            admission_stopped: state.stopped,
        })
    }
}
fn valid_name(
    store: &IndexStore<'_>,
    deadline: Deadline,
    last: &mut Tick,
    meter: &mut Meter,
    budget: &mut td_mime::nfc::HeaderBudget,
    scratch: &mut td_mime::nfc::Scratch,
    name: &str,
) -> Result<bool, ports::Error> {
    crate::mailbox_name::valid(name, meter, budget, scratch, || {
        Ok(check_time(store, deadline, last)?.monotonic)
    })
}
