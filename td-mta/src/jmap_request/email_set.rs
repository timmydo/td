//! The td-mail flag, move and deletion subset of Email/set, with response capacity before effects.
use super::{
    mailbox_get::storage,
    push::{Children, Name},
    Error, Worker,
};
use crate::{
    admission::work::Meter,
    bounded::TextBuffer,
    ids::{EmailId, MailboxId},
    ports,
    store_fs::{
        EmailDestroyCompletion, EmailDestroyRequest, EmailUpdate, EmailUpdateCompletion,
        EmailUpdateRequest, EmailUpdateResult, UploadError, MAX_EMAIL_UPDATES,
    },
    sync::{DataState, DataType},
};
use std::ops::Range;
use td_json::scan;

/// Trusted device mutation adapter; results retain known commit outcomes.
pub trait EmailMutator {
    fn update(
        &self,
        request: EmailUpdateRequest<'_>,
        meter: &mut Meter,
    ) -> Result<EmailUpdateCompletion, UploadError>;
    fn destroy(
        &self,
        _: EmailDestroyRequest<'_>,
        _: &mut Meter,
    ) -> Result<EmailDestroyCompletion, UploadError> {
        Err(ports::Error::Forbidden.into())
    }
}

struct Update {
    name: Name,
    value: EmailUpdate,
    local: bool,
    valid: bool,
    invalid_mailbox: bool,
}
pub(super) fn failure(error: UploadError, conditional: bool) -> Error {
    match error {
        UploadError::Store(ports::Error::Conflict) if conditional => Error::StateMismatch,
        UploadError::Store(ports::Error::Conflict) => Error::StorageBusy,
        UploadError::Store(ports::Error::Forbidden) => Error::Forbidden,
        UploadError::Store(ports::Error::NotFound) => Error::AccountNotFound,
        UploadError::Store(error) => storage(error),
        UploadError::Ledger(_) => Error::StorageBusy,
        UploadError::Format(_) => Error::State,
    }
}
impl Worker<'_, '_, '_, '_, '_> {
    pub(super) fn email_set(
        &mut self,
        arguments: Range<u64>,
        maximum: usize,
        source: td_json::arena::Value<'_>,
    ) -> Result<Range<u64>, Error> {
        let (flags, device) = self.flags.ok_or(Error::UnknownMethod)?;
        let mut fields = Children::new(arguments, scan::Kind::Object)?;
        let (mut account, mut update, mut expected, mut destroy, mut create) =
            (None, None, None, None, None);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(self.push_string(&item)?),
                b"ifInState" if item.kind == scan::Kind::Null => (),
                b"ifInState" => expected = Some(self.push_string(&item)?),
                b"update" => update = Some(item),
                b"create" | b"destroy" if item.kind == scan::Kind::Null => (),
                b"create" => create = Some(item),
                b"destroy" => {
                    if item.kind != scan::Kind::Array {
                        return Err(Error::InvalidArguments);
                    }
                    destroy = Some(item);
                }
                _ => return Err(Error::InvalidArguments),
            }
        }
        let account = account.ok_or(Error::InvalidArguments)?;
        if account.wire_id()?.local() != Some(self.account) {
            return Err(Error::AccountNotFound);
        }
        let expected = expected
            .map(|value| {
                let text =
                    std::str::from_utf8(value.bytes()).map_err(|_| Error::InvalidArguments)?;
                let state = DataState::decode(text).map_err(|_| Error::StateMismatch)?;
                if state.account != self.account || state.kind != DataType::Email {
                    return Err(Error::StateMismatch);
                }
                Ok((state.epoch, state.sequence))
            })
            .transpose()?;
        if let Some(create) = create {
            let mut objects = Children::new(create.range, scan::Kind::Object)?;
            if objects.next(self)?.is_some() {
                for item in [&update, &destroy].into_iter().flatten() {
                    if item.kind != scan::Kind::Null {
                        let kind = if item.key.bytes() == b"update" {
                            scan::Kind::Object
                        } else {
                            scan::Kind::Array
                        };
                        if Children::new(item.range.clone(), kind)?
                            .next(self)?
                            .is_some()
                        {
                            return Err(Error::InvalidArguments);
                        }
                    }
                }
                // td-mail sends literal creation fields. Never substitute unresolved
                // request data for a retained result-reference value.
                let value = source.get("create").ok_or(Error::InvalidArguments)?;
                return self.email_create(value, account, expected, maximum);
            }
        }
        if let Some(destroy) = destroy {
            let mut objects = Children::new(destroy.range.clone(), scan::Kind::Array)?;
            if objects.next(self)?.is_some() {
                if let Some(update) = update.as_ref().filter(|item| item.kind != scan::Kind::Null) {
                    if Children::new(update.range.clone(), scan::Kind::Object)?
                        .next(self)?
                        .is_some()
                    {
                        return Err(Error::InvalidArguments);
                    }
                }
                return self.email_destroy(destroy.range, account, expected, maximum);
            }
        }
        let maximum = maximum.min(MAX_EMAIL_UPDATES);
        let mut updates = Vec::new();
        updates
            .try_reserve_exact(maximum)
            .map_err(|_| Error::ResponseCapacity)?;
        if let Some(update) = update.filter(|item| item.kind != scan::Kind::Null) {
            let mut objects = Children::new(update.range, scan::Kind::Object)?;
            while let Some(item) = objects.next(self)? {
                if updates.len() == maximum {
                    return Err(Error::RequestTooLarge);
                }
                let id: Option<EmailId> = item.key.wire_id()?.local();
                let mut value = EmailUpdate {
                    mailbox: None,
                    email: id.unwrap_or(EmailId::from_bytes([0; 16])),
                    seen: None,
                    flagged: None,
                };
                let mut valid = item.kind == scan::Kind::Object;
                let mut invalid_mailbox = false;
                if valid {
                    let mut patch = Children::new(item.range, scan::Kind::Object)?;
                    while let Some(property) = patch.next(self)? {
                        if property.key.bytes() == b"mailboxIds" {
                            if property.kind != scan::Kind::Object {
                                invalid_mailbox = true;
                                valid = false;
                                continue;
                            }
                            let mut destinations =
                                Children::new(property.range, scan::Kind::Object)?;
                            let mut count = 0;
                            while let Some(destination) = destinations.next(self)? {
                                count += 1;
                                let mailbox: Option<MailboxId> =
                                    destination.key.wire_id().ok().and_then(|id| id.local());
                                if destination.kind != scan::Kind::Boolean
                                    || !self.query_boolean(&destination)?
                                    || mailbox.is_none()
                                {
                                    invalid_mailbox = true;
                                }
                                value.mailbox = mailbox;
                            }
                            invalid_mailbox |= count != 1;
                            valid &= !invalid_mailbox;
                            continue;
                        }
                        let desired = match property.kind {
                            scan::Kind::Boolean if self.query_boolean(&property)? => Some(true),
                            scan::Kind::Null => Some(false),
                            _ => {
                                valid = false;
                                None
                            }
                        };
                        match property.key.bytes() {
                            b"keywords/$seen" => value.seen = desired,
                            b"keywords/$flagged" => value.flagged = desired,
                            _ => valid = false,
                        }
                    }
                }
                updates.push(Update {
                    name: item.key,
                    value,
                    local: id.is_some(),
                    valid,
                    invalid_mailbox,
                });
            }
        }
        // Each ID is validated ASCII with no JSON escaping. Cover either result
        // map plus fixed account/state/framing fields before invoking storage.
        let mut bound = 256 + account.bytes().len() + 2 * DataState::WIRE_BYTES;
        for update in &updates {
            bound = bound
                .checked_add(update.name.bytes().len() + 96)
                .ok_or(Error::ResponseCapacity)?;
        }
        if bound as u64 > self.available()? {
            return Err(Error::ResponseCapacity);
        }
        let mut output = Vec::new();
        output
            .try_reserve_exact(bound)
            .map_err(|_| Error::ResponseCapacity)?;
        output.resize(bound, 0);
        let mut changes = Vec::new();
        changes
            .try_reserve_exact(updates.len())
            .map_err(|_| Error::ResponseCapacity)?;
        for update in &updates {
            if update.valid && update.local {
                changes.push(update.value);
            }
        }
        let reserved = self
            .retention
            .reserve_mutation_output(bound)
            .map_err(Error::Storage)?;
        let request = EmailUpdateRequest {
            account: self.account,
            device,
            expected,
            updates: &changes,
            deadline: self.execution,
        };
        let completion = self
            .retention
            .with_mutation_meter(self.execution, |meter| flags.update(request, meter))
            .map_err(|error| failure(error, expected.is_some()))?;
        // The coordinator already applied admission_stopped to its own state;
        // preserve this call's known result even when future writes must stop.
        let sequence = match completion.outcome {
            Ok(sequence) => sequence,
            Err(crate::store_fs::CommitError::Rejected(error)) => {
                return Err(failure(error.into(), expected.is_some()))
            }
            Err(crate::store_fs::CommitError::Indeterminate(_)) => {
                return Err(Error::ServerPartialFail)
            }
        };
        if completion.results.len() != changes.len() {
            return Err(Error::State);
        }
        let mut before = [0; DataState::WIRE_BYTES];
        let mut after = [0; DataState::WIRE_BYTES];
        let old = DataState {
            account: self.account,
            epoch: completion.before_epoch,
            kind: DataType::Email,
            sequence: completion.before_sequence,
        };
        let new = DataState { sequence, ..old };
        let old = old.encode(&mut before).map_err(|_| Error::State)?;
        let new = new.encode(&mut after).map_err(|_| Error::State)?;
        let account = std::str::from_utf8(account.bytes()).map_err(|_| Error::State)?;
        let mut out = TextBuffer::new(&mut output);
        out.format(format_args!("{{\"accountId\":\"{account}\",\"oldState\":\"{old}\",\"newState\":\"{new}\",\"created\":null,\"destroyed\":null,\"notCreated\":null,\"notDestroyed\":null")).map_err(|_| Error::State)?;
        let successes = completion
            .results
            .iter()
            .filter(|result| **result == EmailUpdateResult::Updated)
            .count();
        for errors in [false, true] {
            let name = if errors { "notUpdated" } else { "updated" };
            out.format(format_args!(",\"{name}\":"))
                .map_err(|_| Error::State)?;
            let count = if errors {
                updates.len() - successes
            } else {
                successes
            };
            if count == 0 {
                out.append("null").map_err(|_| Error::State)?;
                continue;
            }
            out.append("{").map_err(|_| Error::State)?;
            let mut found = completion.results.iter();
            let mut first = true;
            for update in &updates {
                let result = if update.valid && update.local {
                    *found.next().ok_or(Error::State)?
                } else {
                    EmailUpdateResult::NotFound
                };
                let error = if !update.valid {
                    Some("invalidProperties")
                } else {
                    match result {
                        EmailUpdateResult::Updated => None,
                        EmailUpdateResult::NotFound => Some("notFound"),
                        EmailUpdateResult::InvalidMailbox => Some("invalidProperties"),
                    }
                };
                if error.is_some() != errors {
                    continue;
                }
                if !first {
                    out.append(",").map_err(|_| Error::State)?;
                }
                first = false;
                let id = std::str::from_utf8(update.name.bytes()).map_err(|_| Error::State)?;
                out.format(format_args!("\"{id}\":"))
                    .map_err(|_| Error::State)?;
                if let Some(error) = error {
                    let properties = if error == "invalidProperties"
                        && (update.invalid_mailbox || result == EmailUpdateResult::InvalidMailbox)
                    {
                        ",\"properties\":[\"mailboxIds\"]"
                    } else {
                        ""
                    };
                    out.format(format_args!("{{\"type\":\"{error}\"{properties}}}"))
                        .map_err(|_| Error::State)?;
                } else {
                    out.append("null").map_err(|_| Error::State)?;
                }
            }
            out.append("}").map_err(|_| Error::State)?;
        }
        out.append("}").map_err(|_| Error::State)?;
        let length = out.len();
        self.retention
            .finish_mutation_output(reserved, output.get(..length).ok_or(Error::State)?)
            .map_err(Error::Storage)
    }
}

#[path = "email_destroy.rs"]
mod email_destroy;
