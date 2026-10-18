//! Folder creation and singleton deletion for td-mail; output is prepaid.
use super::{
    email_set::failure,
    push::{Children, Item, Name},
    Error, Worker,
};
use crate::{
    admission::work::Meter,
    bounded::TextBuffer,
    store_fs::{
        MailboxCreateCompletion, MailboxCreateRequest, MailboxCreateResult,
        MailboxDeleteCompletion, MailboxDeleteRequest, MailboxDeleteResult, UploadError,
        MAX_MAILBOX_CREATES,
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
    fn delete_mailbox(
        &self,
        request: MailboxDeleteRequest,
        meter: &mut Meter,
    ) -> Result<MailboxDeleteCompletion, UploadError>;
}

struct Creation {
    key: Name,
    name: String,
    slot: usize,
}
impl Worker<'_, '_, '_, '_, '_> {
    pub(super) fn mailbox_set(
        &mut self,
        arguments: Range<u64>,
        maximum: usize,
    ) -> Result<Range<u64>, Error> {
        let (mutator, device) = self.mailboxes.ok_or(Error::UnknownMethod)?;
        let mut fields = Children::new(arguments, scan::Kind::Object)?;
        let (mut account, mut create, mut expected, mut destroy) = (None, None, None, None);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(self.push_string(&item)?),
                b"ifInState" if item.kind == scan::Kind::Null => (),
                b"ifInState" => expected = Some(self.push_string(&item)?),
                b"create" => create = Some(item),
                b"destroy" if item.kind == scan::Kind::Null => (),
                b"destroy" if item.kind == scan::Kind::Array => destroy = Some(item.range),
                b"update" if item.kind == scan::Kind::Null => (),
                b"update" => {
                    if item.kind != scan::Kind::Object
                        || Children::new(item.range, scan::Kind::Object)?
                            .next(self)?
                            .is_some()
                    {
                        return Err(Error::InvalidArguments);
                    }
                }
                b"onDestroyRemoveEmails" if !self.query_boolean(&item)? => (),
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
        if let Some(range) = destroy {
            if Children::new(range.clone(), scan::Kind::Array)?
                .next(self)?
                .is_some()
            {
                if !creates.is_empty() {
                    return Err(Error::InvalidArguments);
                }
                return self.mailbox_destroy(range, account, expected, maximum);
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
            epoch: complete.before_epoch,
            kind: DataType::Mailbox,
            sequence: complete.before_sequence,
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
    fn mailbox_destroy(
        &mut self,
        range: Range<u64>,
        account: Name,
        expected: Option<(crate::ids::StoreEpoch, crate::format::Sequence)>,
        maximum: usize,
    ) -> Result<Range<u64>, Error> {
        let (mutator, device) = self.mailboxes.ok_or(Error::UnknownMethod)?;
        let mut inputs = Children::new(range, scan::Kind::Array)?;
        let item = inputs.next(self)?.ok_or(Error::InvalidArguments)?;
        if maximum == 0 {
            return Err(Error::RequestTooLarge);
        }
        if inputs.next(self)?.is_some() {
            return Err(Error::InvalidArguments);
        }
        let id = self.push_string(&item)?;
        let mailbox = id.wire_id()?.local();
        let bound = 512 + account.bytes().len() + id.bytes().len() + 2 * DataState::WIRE_BYTES;
        if bound as u64 > self.available()? {
            return Err(Error::ResponseCapacity);
        }
        let mut output = Vec::new();
        output
            .try_reserve_exact(bound)
            .map_err(|_| Error::ResponseCapacity)?;
        output.resize(bound, 0);
        let reserved = self
            .retention
            .reserve_mutation_output(bound)
            .map_err(Error::Storage)?;
        let complete = self
            .retention
            .with_mutation_meter(self.execution, |meter| {
                mutator.delete_mailbox(
                    MailboxDeleteRequest {
                        account: self.account,
                        device,
                        expected,
                        mailbox,
                        deadline: self.execution,
                    },
                    meter,
                )
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
            let result = complete.result.ok_or(Error::State)?;
            let old = DataState { account: self.account, epoch: complete.before_epoch,
                kind: DataType::Mailbox, sequence: complete.before_sequence };
            let new = DataState { sequence, ..old };
            let mut before = [0; DataState::WIRE_BYTES];
            let mut after = [0; DataState::WIRE_BYTES];
            let old = old.encode(&mut before).map_err(|_| Error::State)?;
            let new = new.encode(&mut after).map_err(|_| Error::State)?;
            let account = std::str::from_utf8(account.bytes()).map_err(|_| Error::State)?;
            let id = std::str::from_utf8(id.bytes()).map_err(|_| Error::State)?;
            let mut out = TextBuffer::new(&mut output);
            out.format(format_args!("{{\"accountId\":\"{account}\",\"oldState\":\"{old}\",\"newState\":\"{new}\",\"created\":null,\"notCreated\":null,\"updated\":null,\"notUpdated\":null")).map_err(|_| Error::State)?;
            if result == MailboxDeleteResult::Destroyed {
                out.format(format_args!(",\"destroyed\":[\"{id}\"],\"notDestroyed\":null}}")).map_err(|_| Error::State)?;
            } else {
                let kind = match result {
                    MailboxDeleteResult::NotFound => "notFound",
                    MailboxDeleteResult::Forbidden => "forbidden",
                    MailboxDeleteResult::HasChild => "mailboxHasChild",
                    MailboxDeleteResult::HasEmail => "mailboxHasEmail",
                    MailboxDeleteResult::Destroyed => return Err(Error::State),
                };
                out.format(format_args!(",\"destroyed\":null,\"notDestroyed\":{{\"{id}\":{{\"type\":\"{kind}\"}}}}}}")).map_err(|_| Error::State)?;
            }
            let length = out.len();
            self.retention.finish_mutation_output(reserved, output.get(..length).ok_or(Error::State)?).map_err(Error::Storage)
        })().map_err(|error| if matches!(error, Error::State) { Error::ServerPartialFail } else { error })
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
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        admission::{logical::Cell, DiskLimits, ViewMode, WorkLimits},
        format::Sequence,
        ids::{AccountId, DeviceId, MailboxId, StoreEpoch},
        jmap_request::{dispatch, response, CORE, MAIL},
        limits::Limits,
        ownership::SlotState,
        ports::{self, Clock, Deadline, Tick, Time},
        store_fs::{tests::Fixture, RequestSpool, ResponseCoordinator},
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use td_json::{json, Json};
    const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
    const DEVICE: DeviceId = DeviceId::from_bytes([2; 16]);
    const MAILBOX: MailboxId = MailboxId::from_bytes([3; 16]);
    struct Now;
    impl Clock for Now {
        fn sample(&self) -> Result<Time, ports::Error> {
            Ok(Time {
                utc_ms: 1,
                monotonic: Tick(1),
            })
        }
    }
    struct Service {
        calls: AtomicUsize,
        result: MailboxDeleteResult,
        uncertain: bool,
    }
    impl MailboxMutator for Service {
        fn create_mailboxes(
            &self,
            _: MailboxCreateRequest<'_>,
            _: &mut Meter,
        ) -> Result<MailboxCreateCompletion, UploadError> {
            panic!("unexpected creation")
        }
        fn delete_mailbox(
            &self,
            request: MailboxDeleteRequest,
            meter: &mut Meter,
        ) -> Result<MailboxDeleteCompletion, UploadError> {
            assert_eq!(request.account, ACCOUNT);
            assert_eq!(request.device, DEVICE);
            assert_eq!(request.deadline, deadline(90));
            assert_eq!(meter.deadline(), deadline(100).into());
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(MailboxDeleteCompletion {
                before_epoch: StoreEpoch::from_bytes([4; 16]),
                before_sequence: Sequence::from_u64(1),
                outcome: if self.uncertain {
                    Err(crate::store_fs::CommitError::Indeterminate(
                        ports::Error::WriterStopped,
                    ))
                } else {
                    Ok(Sequence::from_u64(
                        if self.result == MailboxDeleteResult::Destroyed {
                            2
                        } else {
                            1
                        },
                    ))
                },
                result: (!self.uncertain).then_some(self.result),
                admission_stopped: self.uncertain,
            })
        }
    }
    fn deadline(n: u64) -> Deadline {
        Deadline::after(Tick(0), n).unwrap()
    }
    fn id(value: impl wire::LocalId) -> String {
        wire::encode_id(value, &mut [0; wire::IdKind::WIRE_BYTES])
            .unwrap()
            .to_owned()
    }
    fn run(mut args: Json, service: &Service) -> Json {
        let _ = args.insert("accountId", id(ACCOUNT));
        let input = json!({"using":[CORE, MAIL], "methodCalls":[["Mailbox/set", args, "call"]]});
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let clock = Arc::new(Now);
        let spool = RequestSpool::open(&mut root, clock.clone(), deadline(100)).unwrap();
        let resources = Limits::default().plan().unwrap();
        let disk = DiskLimits::default()
            .plan(
                &resources,
                WorkLimits::default(),
                ViewMode::OnlineBackground,
            )
            .unwrap();
        let mut slots = [const { SlotState::EMPTY }; 4];
        let mut cells = [const { Cell::EMPTY }; 4];
        let owner = ResponseCoordinator::new(spool, &disk, &mut slots, &mut cells, clock).unwrap();
        let retention = owner.retain_response(deadline(100)).unwrap();
        let request = dispatch::tests::request_with_capabilities(
            &input.to_string(),
            Limits::default(),
            &[CORE, MAIL],
        );
        let mut preflight =
            response::Preflight::new(request, "session", retention.maximum()).unwrap();
        while !preflight.step(&mut [0; 4096]).unwrap() {}
        let mut completed = dispatch::run_with_postgres_mailboxes(
            preflight.finish().unwrap(),
            retention,
            deadline(90),
            ACCOUNT,
            DEVICE,
            service,
        )
        .unwrap();
        let mut encoder = completed.encoder().unwrap();
        while !encoder.count_step(&mut [0; 4096]).unwrap() {}
        let length = encoder.begin_output().unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0; 7];
            let progress = encoder.poll_output(&mut buffer).unwrap();
            bytes.extend_from_slice(&buffer[..progress.written]);
            if progress.complete {
                break;
            }
        }
        assert_eq!(encoder.finish().unwrap(), length);
        td_json::parse_slice(&bytes).unwrap()
    }
    #[test]
    fn delete_refusals_and_unknown_commit_never_emit_destroyed() {
        for (result, kind) in [
            (MailboxDeleteResult::Destroyed, None),
            (MailboxDeleteResult::NotFound, Some("notFound")),
            (MailboxDeleteResult::Forbidden, Some("forbidden")),
            (MailboxDeleteResult::HasChild, Some("mailboxHasChild")),
            (MailboxDeleteResult::HasEmail, Some("mailboxHasEmail")),
        ] {
            let service = Service {
                calls: AtomicUsize::new(0),
                result,
                uncertain: false,
            };
            let response = run(
                json!({"destroy":[id(MAILBOX)], "onDestroyRemoveEmails":false}),
                &service,
            );
            let reply = &response["methodResponses"][0][1];
            assert_eq!(service.calls.load(Ordering::Relaxed), 1);
            if let Some(kind) = kind {
                assert_eq!(
                    reply["notDestroyed"][id(MAILBOX).as_str()]["type"].as_str(),
                    Some(kind)
                );
                assert!(reply["destroyed"].is_null());
                assert_eq!(reply["oldState"], reply["newState"]);
            } else {
                assert_eq!(reply["destroyed"], json!([id(MAILBOX)]));
                assert!(reply["notDestroyed"].is_null());
                assert_ne!(reply["oldState"], reply["newState"]);
            }
        }
        let service = Service {
            calls: AtomicUsize::new(0),
            result: MailboxDeleteResult::Destroyed,
            uncertain: true,
        };
        let response = run(json!({"destroy":[id(MAILBOX)]}), &service);
        assert_eq!(
            response["methodResponses"][0][1]["type"].as_str(),
            Some("serverPartialFail")
        );
        assert!(response["methodResponses"][0][1]["destroyed"].is_null());
    }
    #[test]
    fn invalid_or_unsupported_deletes_refuse_before_mutation() {
        let service = Service {
            calls: AtomicUsize::new(0),
            result: MailboxDeleteResult::Destroyed,
            uncertain: false,
        };
        for args in [
            json!({"destroy":[id(MAILBOX), id(MAILBOX)]}),
            json!({"destroy":[id(MAILBOX)], "create":{"a":{"name":"Other"}}}),
            json!({"destroy":[id(MAILBOX)], "update":{"other":{"name":"Other"}}}),
            json!({"destroy":[id(MAILBOX)],"onDestroyRemoveEmails":true}),
            json!({"destroy":[17]}),
            json!({"destroy":["bad id"]}),
            json!({"destroy":{}}),
        ] {
            let response = run(args, &service);
            assert_eq!(
                response["methodResponses"][0][1]["type"].as_str(),
                Some("invalidArguments"),
                "{response}"
            );
        }
        let response = run(
            json!({"destroy":[id(MAILBOX)],"ifInState":"invalid"}),
            &service,
        );
        assert_eq!(
            response["methodResponses"][0][1]["type"].as_str(),
            Some("stateMismatch")
        );
        assert_eq!(service.calls.load(Ordering::Relaxed), 0);
    }
}
