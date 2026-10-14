//! Root-folder creation used by td-mail; complete output is prepaid.
use super::{
    email_set::failure,
    push::{Children, Item, Name},
    Error, Worker,
};
use crate::{
    admission::work::Meter,
    bounded::TextBuffer,
    store_fs::{
        MailboxAuthorization, MailboxCreateCompletion, MailboxCreateRequest, MailboxCreateResult,
        StoreCoordinator, UploadError, MAX_MAILBOX_CREATES,
    },
    sync::{DataState, DataType},
    wire,
};
use std::ops::Range;
use td_json::{arena, scan};

pub trait MailboxMutator {
    fn create_mailboxes(
        &self,
        request: MailboxCreateRequest<'_>,
        meter: &mut Meter,
    ) -> Result<MailboxCreateCompletion, UploadError>;
}
struct FolderEntropy(Option<td_crypto::SystemEntropy>);
impl td_crypto::Entropy for FolderEntropy {
    fn fill(&mut self, output: &mut [u8]) -> Result<(), td_crypto::Error> {
        if self.0.is_none() {
            self.0 = Some(td_crypto::SystemEntropy::try_new()?);
        }
        self.0
            .as_mut()
            .ok_or(td_crypto::Error::Entropy)?
            .fill(output)
    }
}
impl<P: MailboxAuthorization> MailboxMutator for StoreCoordinator<'_, '_, P> {
    fn create_mailboxes(
        &self,
        request: MailboxCreateRequest<'_>,
        meter: &mut Meter,
    ) -> Result<MailboxCreateCompletion, UploadError> {
        let mut entropy = FolderEntropy(None);
        self.create_mailboxes(&td_crypto::Provider, &mut entropy, request, meter)
    }
}
struct Creation {
    key: Name,
    name: String,
    slot: usize,
}
impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    pub(super) fn mailbox_set(
        &mut self,
        arguments: Range<u64>,
        maximum: usize,
    ) -> Result<Range<u64>, Error> {
        let (mutator, device) = self.mailboxes.ok_or(Error::UnknownMethod)?;
        let mut fields = Children::new(arguments, scan::Kind::Object)?;
        let (mut account, mut create, mut expected) = (None, None, None);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(self.push_string(&item)?),
                b"ifInState" if item.kind == scan::Kind::Null => (),
                b"ifInState" => expected = Some(self.push_string(&item)?),
                b"create" => create = Some(item),
                b"update" | b"destroy" if item.kind == scan::Kind::Null => (),
                b"update" | b"destroy" => {
                    let kind = if item.key.bytes() == b"update" {
                        scan::Kind::Object
                    } else {
                        scan::Kind::Array
                    };
                    if item.kind != kind || Children::new(item.range, kind)?.next(self)?.is_some() {
                        return Err(Error::InvalidArguments);
                    }
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
                if state.account != self.account || state.kind != DataType::Mailbox {
                    return Err(Error::StateMismatch);
                }
                Ok((state.epoch, state.sequence))
            })
            .transpose()?;
        let maximum = maximum.min(MAX_MAILBOX_CREATES);
        let mut creates = Vec::new();
        creates
            .try_reserve_exact(maximum)
            .map_err(|_| Error::ResponseCapacity)?;
        if let Some(create) = create.filter(|item| item.kind != scan::Kind::Null) {
            if create.kind != scan::Kind::Object {
                return Err(Error::InvalidArguments);
            }
            let mut objects = Children::new(create.range, scan::Kind::Object)?;
            while let Some(item) = objects.next(self)? {
                if creates.len() == maximum {
                    return Err(Error::RequestTooLarge);
                }
                item.key.id()?;
                let mut name = String::new();
                let mut valid = item.kind == scan::Kind::Object;
                if valid {
                    let mut properties = Children::new(item.range, scan::Kind::Object)?;
                    while let Some(property) = properties.next(self)? {
                        if property.key.bytes() == b"name" {
                            match self.mailbox_name(&property)? {
                                Some(value) => name = value,
                                None => valid = false,
                            }
                        } else {
                            valid = false;
                        }
                    }
                }
                if !valid {
                    name.clear();
                }
                // Empty is the native invalid-name marker, never a created folder.
                creates.push(Creation {
                    slot: 0,
                    key: item.key,
                    name,
                });
            }
        }
        let mut bound = 256 + account.bytes().len() + 2 * DataState::WIRE_BYTES;
        for create in &creates {
            bound = bound
                .checked_add(create.key.bytes().len() + 160)
                .ok_or(Error::ResponseCapacity)?;
        }
        if bound as u64 > self.available()? {
            return Err(Error::ResponseCapacity);
        }
        if creates.len() > crate::jmap_request::creation_ids::MAX_CREATED {
            return Err(Error::ResponseCapacity);
        }
        for create in &mut creates {
            create.slot = self.prepare_creation(create.key.bytes())?;
        }
        if bound as u64 > self.available()? {
            return Err(Error::ResponseCapacity);
        }
        let mut output = Vec::new();
        output
            .try_reserve_exact(bound)
            .map_err(|_| Error::ResponseCapacity)?;
        output.resize(bound, 0);
        let mut names = Vec::new();
        names
            .try_reserve_exact(creates.len())
            .map_err(|_| Error::ResponseCapacity)?;
        for create in &creates {
            names.push(create.name.as_str());
        }
        let reserved = self
            .retention
            .reserve_mutation_output(bound)
            .map_err(Error::Storage)?;
        let request = MailboxCreateRequest {
            account: self.account,
            device,
            expected,
            names: &names,
            deadline: self.execution,
        };
        let complete = self
            .retention
            .with_mutation_meter(self.execution, |meter| {
                mutator.create_mailboxes(request, meter)
            })
            .map_err(|error| failure(error, expected.is_some()))?;
        let sequence = match complete.outcome {
            Ok(sequence) => sequence,
            Err(crate::store_fs::CommitError::Rejected(error)) => {
                return Err(failure(error.into(), expected.is_some()))
            }
            Err(crate::store_fs::CommitError::Indeterminate(_)) => {
                return Err(Error::ServerPartialFail)
            }
        };
        (|| {
        if complete.results.len() != creates.len() {
            return Err(Error::State);
        }
        let old = DataState {
            account: self.account,
            epoch: complete.before.epoch,
            kind: DataType::Mailbox,
            sequence: complete.before.committed_sequence,
        };
        let new = DataState { sequence, ..old };
        let mut before = [0; DataState::WIRE_BYTES];
        let mut after = [0; DataState::WIRE_BYTES];
        let old = old.encode(&mut before).map_err(|_| Error::State)?;
        let new = new.encode(&mut after).map_err(|_| Error::State)?;
        let account = std::str::from_utf8(account.bytes()).map_err(|_| Error::State)?;
        let mut out = TextBuffer::new(&mut output);
        out.format(format_args!("{{\"accountId\":\"{account}\",\"oldState\":\"{old}\",\"newState\":\"{new}\",\"updated\":null,\"notUpdated\":null,\"destroyed\":null,\"notDestroyed\":null")).map_err(|_| Error::State)?;
        for errors in [false, true] {
            let field = if errors { "notCreated" } else { "created" };
            out.format(format_args!(",\"{field}\":"))
                .map_err(|_| Error::State)?;
            let mut first = true;
            for (create, result) in creates.iter().zip(&complete.results) {
                if matches!(result, MailboxCreateResult::Created(_)) == errors {
                    continue;
                }
                out.append(if first { "{" } else { "," })
                    .map_err(|_| Error::State)?;
                first = false;
                let key = std::str::from_utf8(create.key.bytes()).map_err(|_| Error::State)?;
                out.format(format_args!("\"{key}\":"))
                    .map_err(|_| Error::State)?;
                match result {
                    MailboxCreateResult::Created(id) => {
                        self.created.created(create.slot, *id)?;
                        let mut bytes = [0; wire::IdKind::WIRE_BYTES];
                        let id = wire::encode_id(*id, &mut bytes).map_err(|_| Error::State)?;
                        out.format(format_args!("{{\"id\":\"{id}\"}}"))
                            .map_err(|_| Error::State)?;
                    }
                    MailboxCreateResult::InvalidName => out
                        .append("{\"type\":\"invalidProperties\"}")
                        .map_err(|_| Error::State)?,
                    MailboxCreateResult::AlreadyExists => out
                        .append("{\"type\":\"invalidProperties\",\"properties\":[\"name\"]}")
                        .map_err(|_| Error::State)?,
                }
            }
            out.append(if first { "null" } else { "}" })
                .map_err(|_| Error::State)?;
        }
        out.append("}").map_err(|_| Error::State)?;
        let length = out.len();
        self.retention
            .finish_mutation_output(reserved, output.get(..length).ok_or(Error::State)?)
            .map_err(Error::Storage)
        })().map_err(|error| match error {
            Error::State => Error::ServerPartialFail,
            other => other,
        })
    }
    fn mailbox_name(&mut self, item: &Item) -> Result<Option<String>, Error> {
        const ENCODED: usize = crate::format::row::MAX_MAILBOX_NAME * 6 + 2;
        if item.kind != scan::Kind::String {
            return Ok(None);
        }
        let length = item
            .range
            .end
            .checked_sub(item.range.start)
            .ok_or(Error::State)?;
        if length > ENCODED as u64 {
            return Ok(None);
        }
        let mut bytes = [0; ENCODED];
        let input = bytes.get_mut(..length as usize).ok_or(Error::State)?;
        self.check()?;
        self.retention
            .read_at(item.range.start, input)
            .map_err(Error::Storage)?;
        let mut tokens = [arena::Token::EMPTY; 1];
        let document = arena::parse(
            input,
            &mut tokens,
            arena::ParseLimits {
                max_bytes: ENCODED,
                max_depth: 1,
                max_tokens: 1,
            },
        )
        .map_err(|_| Error::State)?;
        let value = document.root().as_string_bytes().ok_or(Error::State)?;
        if value.len() > crate::format::row::MAX_MAILBOX_NAME {
            return Ok(None);
        }
        let value = std::str::from_utf8(value).map_err(|_| Error::State)?;
        let mut name = String::new();
        name.try_reserve_exact(value.len())
            .map_err(|_| Error::ResponseCapacity)?;
        name.push_str(value);
        self.check()?;
        Ok(Some(name))
    }
}

#[cfg(test)]
#[path = "mailbox_set_tests.rs"]
mod tests;
