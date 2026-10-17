//! One literal plain-text draft per td-mail Email/set call.
use super::{email_set::failure, push::Name, Error, Worker};
use crate::{
    admission::work::Meter,
    bounded::TextBuffer,
    draft_input,
    format::Sequence,
    ids::{AccountId, DeviceId, StoreEpoch},
    ports,
    store_fs::{CommitError, DraftCompletion, UploadError},
    sync::{DataState, DataType},
    wire,
};
use std::ops::Range;
use td_json::arena::Value;

pub struct DraftInput<'a> {
    pub account: AccountId,
    pub device: DeviceId,
    pub expected: Option<(StoreEpoch, Sequence)>,
    pub deadline: ports::Deadline,
    pub value: Value<'a>,
}
#[derive(Debug)]
pub enum DraftError {
    Input(draft_input::Error),
    Store(UploadError),
}
/// Trusted adapter; the complete response is prepaid before this call. Parsing,
/// composition and publication must spend this original request meter. A
/// prepublication Store(Quota) means the composed message exceeds its size
/// ceiling; account quota admission uses UploadError::Ledger instead.
pub trait DraftCreator: Sync {
    fn create_draft(
        &self,
        input: DraftInput<'_>,
        meter: &mut Meter,
    ) -> Result<DraftCompletion, DraftError>;
}
impl Worker<'_, '_, '_, '_, '_> {
    pub(super) fn email_create(
        &mut self,
        create: Value<'_>,
        account: Name,
        expected: Option<(StoreEpoch, Sequence)>,
        maximum: usize,
    ) -> Result<Range<u64>, Error> {
        let access = self.drafts.ok_or(Error::InvalidArguments)?;
        let creator = access.creator;
        let device = access.device;
        let mut objects = create.as_object().ok_or(Error::InvalidArguments)?;
        if objects.len() > maximum {
            return Err(Error::RequestTooLarge);
        }
        if objects.len() != 1 {
            return Err(Error::InvalidArguments);
        }
        self.check()?;
        let (key, value) = objects.next_bytes().ok_or(Error::InvalidArguments)?;
        let key = std::str::from_utf8(key).map_err(|_| Error::InvalidArguments)?;
        wire::Id::parse(key).map_err(|_| Error::InvalidArguments)?;
        let bound = 512 + key.len() + account.bytes().len() + 2 * DataState::WIRE_BYTES;
        if bound as u64 > self.available()? {
            return Err(Error::ResponseCapacity);
        }
        let mut output = Vec::new();
        output
            .try_reserve_exact(bound)
            .map_err(|_| Error::ResponseCapacity)?;
        output.resize(bound, 0);
        let before = access.state.ok_or(Error::State)?;
        if before.account != self.account || before.kind != DataType::Email {
            return Err(Error::State);
        }
        if expected.is_some_and(|state| state != (before.epoch, before.sequence)) {
            return Err(Error::StateMismatch);
        }
        let creation = self.prepare_creation(key.as_bytes())?;
        if bound as u64 > self.available()? {
            return Err(Error::ResponseCapacity);
        }
        let reserved = self
            .retention
            .reserve_mutation_output(bound)
            .map_err(Error::Storage)?;
        let result = self
            .retention
            .with_mutation_meter(self.execution, |meter| {
                Ok(creator.create_draft(
                    DraftInput {
                        account: self.account,
                        device,
                        expected,
                        deadline: self.execution,
                        value,
                    },
                    meter,
                ))
            })
            .map_err(|error| failure(error, expected.is_some()))?;
        let (before, sequence, created, property, error_type) = match result {
            Ok(complete) => {
                let sequence = match complete.outcome {
                    Ok(sequence) => sequence,
                    Err(CommitError::Rejected(error)) => {
                        return Err(failure(error.into(), expected.is_some()))
                    }
                    Err(CommitError::Indeterminate(_)) => return Err(Error::ServerPartialFail),
                };
                if let Some(access) = self.submissions.as_mut() {
                    access.state.epoch = complete.before_epoch;
                    access.state.sequence = sequence;
                }
                (
                    DataState {
                        epoch: complete.before_epoch,
                        sequence: complete.before_sequence,
                        ..before
                    },
                    sequence,
                    Some(complete),
                    None,
                    "",
                )
            }
            Err(DraftError::Input(draft_input::Error::Property(property))) => (
                before,
                before.sequence,
                None,
                Some(property),
                "invalidProperties",
            ),
            Err(DraftError::Input(draft_input::Error::Deadline)) => {
                return Err(Error::ExecutionDeadline)
            }
            Err(DraftError::Input(draft_input::Error::Capacity)) => {
                return Err(Error::StorageCapacity)
            }
            Err(DraftError::Input(draft_input::Error::Invalid | draft_input::Error::Clock)) => {
                return Err(Error::State)
            }
            Err(DraftError::Store(UploadError::Store(
                ports::Error::NotFound | ports::Error::Invalid,
            ))) => {
                // These codes also describe clock, spool and store failures;
                // they cannot establish which client property was invalid.
                (before, before.sequence, None, None, "serverFail")
            }
            Err(DraftError::Store(UploadError::Store(ports::Error::Quota))) => {
                (before, before.sequence, None, None, "tooLarge")
            }
            Err(DraftError::Store(error)) => return Err(failure(error, expected.is_some())),
        };
        (|| {
            let old = DataState { account: self.account, epoch: before.epoch, kind: DataType::Email, sequence: before.sequence };
            let new = DataState { sequence, ..old };
            let mut old_bytes = [0; DataState::WIRE_BYTES];
            let mut new_bytes = [0; DataState::WIRE_BYTES];
            let old = old.encode(&mut old_bytes).map_err(|_| Error::State)?;
            let new = new.encode(&mut new_bytes).map_err(|_| Error::State)?;
            let account = std::str::from_utf8(account.bytes()).map_err(|_| Error::State)?;
            let mut out = TextBuffer::new(&mut output);
            out.format(format_args!("{{\"accountId\":\"{account}\",\"oldState\":\"{old}\",\"newState\":\"{new}\",\"updated\":null,\"notUpdated\":null,\"destroyed\":null,\"notDestroyed\":null")).map_err(|_| Error::State)?;
            if let Some(complete) = created {
                self.created.created(creation, complete.email)?;
                let mut email = [0; wire::IdKind::WIRE_BYTES];
                let mut blob = [0; wire::IdKind::WIRE_BYTES];
                let mut thread = [0; wire::IdKind::WIRE_BYTES];
                let email = wire::encode_id(complete.email, &mut email).map_err(|_| Error::State)?;
                let blob = wire::encode_id(complete.blob, &mut blob).map_err(|_| Error::State)?;
                let thread = wire::encode_id(complete.thread, &mut thread).map_err(|_| Error::State)?;
                out.format(format_args!(" ,\"created\":{{\"{key}\":{{\"id\":\"{email}\",\"blobId\":\"{blob}\",\"threadId\":\"{thread}\",\"size\":{}}}}},\"notCreated\":null}}", complete.size)).map_err(|_| Error::State)?;
            } else {
                out.format(format_args!(",\"created\":null,\"notCreated\":{{\"{key}\":{{\"type\":\"{error_type}\"")).map_err(|_| Error::State)?;
                // The adapter supplies only fixed decoder property names. An
                // unknown property or composition refusal has no guessed path.
                if let Some(property) = property.filter(|name| *name != "create") {
                    if !property.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
                        return Err(Error::State);
                    }
                    out.format(format_args!(",\"properties\":[\"{property}\"]")).map_err(|_| Error::State)?;
                }
                out.append("}}}").map_err(|_| Error::State)?;
            }
            let length = out.len();
            self.retention.finish_mutation_output(reserved, output.get(..length).ok_or(Error::State)?).map_err(Error::Storage)
        })().map_err(|error| if error == Error::State { Error::ServerPartialFail } else { error })
    }
}
