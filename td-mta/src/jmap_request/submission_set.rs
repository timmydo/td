//! One prepared submission plus its independently reported success filing.
use super::{Error, Worker};
use crate::{
    admission::work::Meter,
    bounded::TextBuffer,
    format::Sequence,
    ids::DeviceId,
    ports::{self, Clock},
    postgres_http_submissions as sending, postgres_http_updates as filing,
    postgres_submissions as submissions,
    store_fs::{CommitError, EmailUpdateCompletion, EmailUpdateResult, UploadError},
    submission_request,
    sync::{DataState, DataType},
    wire,
};
use std::ops::Range;

/// Trusted fixed-worker adapter. Publication and preparation spend the supplied
/// original meter; an indeterminate result must retain its operation fence.
pub trait SubmissionCreator {
    fn recipient_maximum(&self) -> usize;
    fn create(
        &self,
        input: sending::Input,
        meter: &mut Meter,
    ) -> Result<submissions::Outcome, sending::Error>;
}
impl SubmissionCreator for sending::Submissions<'_> {
    fn recipient_maximum(&self) -> usize {
        self.binding.recipient_maximum()
    }
    fn create(
        &self,
        input: sending::Input,
        meter: &mut Meter,
    ) -> Result<submissions::Outcome, sending::Error> {
        sending::Submissions::create(self, input, meter)
    }
}
pub trait SubmissionFiler {
    fn file(
        &self,
        input: filing::FilingRequest,
        meter: &mut Meter,
    ) -> Result<EmailUpdateCompletion, UploadError>;
}
impl SubmissionFiler for filing::Updates<'_> {
    fn file(
        &self,
        input: filing::FilingRequest,
        meter: &mut Meter,
    ) -> Result<EmailUpdateCompletion, UploadError> {
        filing::Updates::file(self, input, meter)
    }
}
#[derive(Clone, Copy)]
pub struct SubmissionAccess<'a> {
    pub creator: &'a dyn SubmissionCreator,
    pub filer: &'a dyn SubmissionFiler,
    pub clock: &'a dyn Clock,
    pub device: DeviceId,
    pub state: DataState,
}
impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    pub(super) fn submission_set(
        &mut self,
        call: super::super::Invocation<'_>,
        request: super::super::Request<'_>,
        maximum: usize,
    ) -> Result<Range<u64>, Error> {
        let access = self.submissions.ok_or(Error::State)?;
        let mut fields = call.arguments.as_object().ok_or(Error::InvalidArguments)?;
        let (mut account, mut create, mut hook, mut expected) = (None, None, None, None);
        while let Some((key, value)) = fields.next_bytes() {
            self.check()?;
            match key {
                b"accountId" => account = Some(value),
                b"create" => create = Some(value),
                b"ifInState" if value.is_null() => (),
                b"ifInState" => expected = Some(value),
                b"onSuccessUpdateEmail" if value.is_null() => (),
                b"onSuccessUpdateEmail" => hook = Some(value),
                _ => return Err(Error::InvalidArguments),
            }
        }
        let account = account
            .and_then(|value| value.as_string_bytes())
            .ok_or(Error::InvalidArguments)?;
        let account = std::str::from_utf8(account).map_err(|_| Error::InvalidArguments)?;
        if wire::Id::parse(account)
            .map_err(|_| Error::InvalidArguments)?
            .local()
            != Some(self.account)
        {
            return Err(Error::AccountNotFound);
        }
        let expected = expected
            .map(|value| {
                let text = value
                    .as_string_bytes()
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .ok_or(Error::InvalidArguments)?;
                let state = DataState::decode(text).map_err(|_| Error::StateMismatch)?;
                if state.account != self.account || state.kind != DataType::Submission {
                    return Err(Error::StateMismatch);
                }
                Ok((state.epoch, state.sequence.number()))
            })
            .transpose()?;
        let mut objects = create
            .and_then(|value| value.as_object())
            .ok_or(Error::InvalidArguments)?;
        if objects.len() > maximum {
            return Err(Error::RequestTooLarge);
        }
        if objects.len() != 1 {
            return Err(Error::InvalidArguments);
        }
        let (key, value) = objects.next_bytes().ok_or(Error::InvalidArguments)?;
        let key_text = std::str::from_utf8(key).map_err(|_| Error::InvalidArguments)?;
        wire::Id::parse(key_text).map_err(|_| Error::InvalidArguments)?;
        let hook = hook
            .map(|value| {
                let mut entries = value.as_object().ok_or(Error::InvalidArguments)?;
                if entries.len() != 1 {
                    return Err(Error::InvalidArguments);
                }
                let (hook_key, value) = entries.next_bytes().ok_or(Error::InvalidArguments)?;
                if hook_key.strip_prefix(b"#") != Some(key) {
                    return Err(Error::InvalidArguments);
                }
                Ok(value)
            })
            .transpose()?;
        let decoded = self
            .retention
            .with_mutation_meter(self.execution, |meter| {
                let creation =
                    submission_request::creation(value, access.clock, self.execution, meter);
                let hook = hook
                    .map(|value| {
                        submission_request::filing(value, access.clock, self.execution, meter)
                    })
                    .transpose();
                Ok((creation, hook))
            })
            .map_err(|error| super::email_set::failure(error, expected.is_some()))?;
        let creation = decoded.0.map_err(input_error)?;
        let hook = decoded.1.map_err(input_error)?;
        let email = match creation.email {
            submission_request::Email::Id(id) => id.local(),
            submission_request::Email::Created(id) => {
                let name = self.created.lookup(id.as_str().as_bytes(), || {
                    if self
                        .retention
                        .admit_before(self.execution)
                        .map_err(Error::Storage)?
                    {
                        Ok(())
                    } else {
                        Err(Error::ExecutionDeadline)
                    }
                })?;
                let overlay_present = name.is_some();
                let mut found = name.and_then(|value| {
                    std::str::from_utf8(value.bytes())
                        .ok()
                        .and_then(|text| wire::Id::parse(text).ok())
                        .and_then(|id| id.local())
                });
                if !overlay_present {
                    if let Some(mut entries) = request.created_ids() {
                        while let Some((key, value)) = entries.next_bytes() {
                            self.check()?;
                            if key == id.as_str().as_bytes() {
                                found = value
                                    .as_string_bytes()
                                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                                    .and_then(|text| wire::Id::parse(text).ok())
                                    .and_then(|id| id.local());
                                break;
                            }
                        }
                    }
                }
                found
            }
        }
        .ok_or(Error::InvalidArguments)?;
        let identity = creation.identity.local().ok_or(Error::InvalidArguments)?;
        let movement = hook.map(|hook| match (hook.source, hook.destination) {
            (None, None) => Ok(None),
            (Some(source), Some(destination)) => {
                source.local().zip(destination.local()).map(Some).ok_or(())
            }
            _ => Err(()),
        });
        let slot = self.prepare_creation(key)?;
        let bound = 4096usize
            .checked_add(key.len())
            .ok_or(Error::ResponseCapacity)?;
        let frame = if hook.is_some() {
            call.call_id
                .len()
                .checked_mul(6)
                .and_then(|bytes| bytes.checked_add(64))
                .ok_or(Error::ResponseCapacity)?
        } else {
            0
        };
        let available = self.available()?;
        if (bound as u64)
            .checked_add(frame as u64)
            .is_none_or(|needed| needed > available)
        {
            return Err(Error::ResponseCapacity);
        }
        self.optional_end = self
            .optional_end
            .checked_sub(frame as u64)
            .ok_or(Error::ResponseCapacity)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(bound)
            .map_err(|_| Error::ResponseCapacity)?;
        bytes.resize(bound, 0);
        let mut filing_bytes = [0; 2048];
        let reserved = self
            .retention
            .reserve_mutation_output(bound)
            .map_err(Error::Storage)?;
        let result = self
            .retention
            .with_mutation_meter(self.execution, |meter| {
                Ok(access.creator.create(
                    sending::Input {
                        account: self.account,
                        device: access.device,
                        email,
                        identity,
                        expected,
                        deadline: self.execution,
                    },
                    meter,
                ))
            })
            .map_err(|error| super::email_set::failure(error, expected.is_some()))?;
        let result = match result {
            Err(sending::Error::Publication(submissions::Error::Indeterminate)) => {
                return Err(Error::ServerPartialFail)
            }
            Err(
                sending::Error::Publication(submissions::Error::State)
                | sending::Error::Input(crate::postgres_submission_input::Error::State)
                | sending::Error::Admission(ports::Error::Conflict),
            ) if expected.is_some() => return Err(Error::StateMismatch),
            Err(error) => {
                if let Some(method) = creation_method_error(error) {
                    return Err(method);
                }
                Err(error)
            }
            other => other,
        };
        let mut output = TextBuffer::new(&mut bytes);
        let mut implicit = None;
        match result {
            Ok(complete) => {
                if complete.email != email
                    || complete.new_sequence.checked_sub(complete.old_sequence) != Some(1)
                {
                    return Err(Error::ServerPartialFail);
                }
                self.created
                    .created(slot, complete.submission)
                    .map_err(|_| Error::ServerPartialFail)?;
                let old = DataState {
                    sequence: Sequence::from_u64(complete.old_sequence),
                    ..access.state
                };
                let new = DataState {
                    sequence: Sequence::from_u64(complete.new_sequence),
                    ..old
                };
                states(&mut output, old, new)?;
                let mut encoded = [0; wire::IdKind::WIRE_BYTES];
                let id = wire::encode_id(complete.submission, &mut encoded)
                    .map_err(|_| Error::ServerPartialFail)?;
                output.format(format_args!(",\"created\":{{\"{key_text}\":{{\"id\":\"{id}\"}}}},\"notCreated\":null,\"updated\":null,\"notUpdated\":null,\"destroyed\":null,\"notDestroyed\":null}}")).map_err(|_| Error::ServerPartialFail)?;
                let primary = output.len();
                if let Some(movement) = movement {
                    let filed = match movement {
                        Ok(movement) => {
                            Some(self.retention.with_mutation_meter(self.execution, |meter| {
                                access.filer.file(
                                    filing::FilingRequest {
                                        account: self.account,
                                        device: access.device,
                                        email,
                                        movement,
                                        deadline: self.execution,
                                    },
                                    meter,
                                )
                            }))
                        }
                        Err(()) => None,
                    };
                    let mut filing_output = TextBuffer::new(&mut filing_bytes);
                    let result = match filed {
                        Some(filed) => write_filing(&mut filing_output, filed, access.state, email),
                        None => write_unknown_filing(&mut filing_output, new, email),
                    };
                    let name = match result {
                        Ok(name) => name,
                        Err(_) => {
                            filing_output.clear();
                            filing_output
                                .append("{\"type\":\"serverPartialFail\"}")
                                .map_err(|_| Error::ServerPartialFail)?;
                            "error"
                        }
                    };
                    output
                        .append(
                            filing_output
                                .as_str()
                                .map_err(|_| Error::ServerPartialFail)?,
                        )
                        .map_err(|_| Error::ServerPartialFail)?;
                    implicit = Some((name, primary));
                }
            }
            Err(error) => {
                states(&mut output, access.state, access.state)?;
                output
                    .format(format_args!(
                        ",\"created\":null,\"notCreated\":{{\"{key_text}\":"
                    ))
                    .map_err(|_| Error::ServerPartialFail)?;
                write_creation_error(&mut output, error, access.creator.recipient_maximum())?;
                output.append("},\"updated\":null,\"notUpdated\":null,\"destroyed\":null,\"notDestroyed\":null}")
                    .map_err(|_| Error::ServerPartialFail)?;
            }
        }
        let length = output.len();
        let range = self
            .retention
            .finish_mutation_output(
                reserved,
                bytes.get(..length).ok_or(Error::ServerPartialFail)?,
            )
            .map_err(Error::Storage)?;
        if let Some((name, length)) = implicit {
            let split = range.start.checked_add(length as u64).ok_or(Error::State)?;
            self.implicit = Some((name, split..range.end));
            Ok(range.start..split)
        } else {
            Ok(range)
        }
    }
}
fn creation_method_error(error: sending::Error) -> Option<Error> {
    use crate::{
        postgres_probe::Error as Connection, postgres_read::Error as Read,
        postgres_submission_input::Error as Input, submission_envelope::Error as Envelope,
    };
    let port = match error {
        sending::Error::Admission(error)
        | sending::Error::Input(Input::Work(error) | Input::Envelope(Envelope::Policy(error)))
        | sending::Error::Publication(submissions::Error::Work(error)) => Some(error),
        sending::Error::Input(Input::Read(
            Read::Output(error) | Read::Body(crate::postgres_body::Error::Read(error)),
        )) => Some(error),
        _ => None,
    };
    if let Some(error) = port {
        return match error {
            ports::Error::Deadline => Some(Error::ExecutionDeadline),
            ports::Error::Capacity => Some(Error::StorageCapacity),
            ports::Error::Busy
            | ports::Error::Dns
            | ports::Error::Tls
            | ports::Error::Io { .. } => Some(Error::StorageBusy),
            _ => None,
        };
    }
    match error {
        sending::Error::Input(Input::Read(
            Read::Body(crate::postgres_body::Error::Connection(Connection::Deadline))
            | Read::Connection(Connection::Deadline),
        ))
        | sending::Error::Publication(submissions::Error::Connection(Connection::Deadline)) => {
            Some(Error::ExecutionDeadline)
        }
        sending::Error::Input(Input::Read(
            Read::Busy
            | Read::Body(crate::postgres_body::Error::Connection(
                Connection::Transport | Connection::Tls | Connection::Database,
            ))
            | Read::Resolution(_)
            | Read::Connection(Connection::Transport | Connection::Tls | Connection::Database),
        ))
        | sending::Error::Publication(submissions::Error::Connection(
            Connection::Transport | Connection::Tls | Connection::Database,
        )) => Some(Error::StorageBusy),
        sending::Error::Input(
            Input::Capacity
            | Input::Read(Read::Capacity | Read::Body(crate::postgres_body::Error::Capacity)),
        )
        | sending::Error::Publication(submissions::Error::Capacity) => Some(Error::StorageCapacity),
        _ => None,
    }
}
fn write_creation_error(
    output: &mut TextBuffer<'_>,
    error: sending::Error,
    maximum: usize,
) -> Result<(), Error> {
    use crate::{
        postgres_submission_input::Error as Input,
        submission_envelope::{Error as Envelope, Field},
    };
    let (kind, properties) = match error {
        sending::Error::Publication(submissions::Error::Quota) => ("overQuota", None),
        sending::Error::Input(Input::Identity) => ("invalidProperties", Some("identityId")),
        sending::Error::Input(Input::Read(crate::postgres_read::Error::Body(
            crate::postgres_body::Error::Missing,
        ))) => ("invalidProperties", Some("emailId")),
        sending::Error::Input(Input::Envelope(Envelope::NoRecipients)) => ("noRecipients", None),
        sending::Error::Input(Input::Envelope(Envelope::ForbiddenFrom)) => ("forbiddenFrom", None),
        sending::Error::Input(Input::Envelope(Envelope::TooManyRecipients)) => {
            return output
                .format(format_args!(
                    "{{\"type\":\"tooManyRecipients\",\"maxRecipients\":{maximum}}}"
                ))
                .map_err(|_| Error::ServerPartialFail);
        }
        sending::Error::Input(Input::Envelope(Envelope::InvalidEmail(field))) => (
            "invalidEmail",
            Some(match field {
                Field::From => "from",
                Field::Sender => "sender",
                Field::To => "to",
                Field::Cc => "cc",
                Field::Bcc => "bcc",
            }),
        ),
        sending::Error::Input(Input::Headers | Input::Envelope(Envelope::Headers)) => {
            ("invalidEmail", None)
        }
        _ => ("serverFail", None),
    };
    output
        .format(format_args!("{{\"type\":\"{kind}\""))
        .map_err(|_| Error::ServerPartialFail)?;
    if let Some(property) = properties {
        output
            .format(format_args!(",\"properties\":[\"{property}\"]"))
            .map_err(|_| Error::ServerPartialFail)?;
    } else if kind == "invalidEmail" {
        output
            .append(",\"properties\":[]")
            .map_err(|_| Error::ServerPartialFail)?;
    }
    output.append("}").map_err(|_| Error::ServerPartialFail)
}
fn input_error(error: submission_request::Error) -> Error {
    match error {
        submission_request::Error::Property(_) => Error::InvalidArguments,
        submission_request::Error::Capacity => Error::StorageCapacity,
        submission_request::Error::Deadline => Error::ExecutionDeadline,
        submission_request::Error::Clock => Error::State,
    }
}
fn states(output: &mut TextBuffer<'_>, old: DataState, new: DataState) -> Result<(), Error> {
    let mut account = [0; wire::IdKind::WIRE_BYTES];
    let account =
        wire::encode_id(old.account, &mut account).map_err(|_| Error::ServerPartialFail)?;
    let mut old_bytes = [0; DataState::WIRE_BYTES];
    let mut new_bytes = [0; DataState::WIRE_BYTES];
    let old = old
        .encode(&mut old_bytes)
        .map_err(|_| Error::ServerPartialFail)?;
    let new = new
        .encode(&mut new_bytes)
        .map_err(|_| Error::ServerPartialFail)?;
    output
        .format(format_args!(
            "{{\"accountId\":\"{account}\",\"oldState\":\"{old}\",\"newState\":\"{new}\""
        ))
        .map_err(|_| Error::ServerPartialFail)
}
fn write_unknown_filing(
    output: &mut TextBuffer<'_>,
    state: DataState,
    email: crate::ids::EmailId,
) -> Result<&'static str, Error> {
    let state = DataState {
        kind: DataType::Email,
        ..state
    };
    states(output, state, state)?;
    let mut bytes = [0; wire::IdKind::WIRE_BYTES];
    let id = wire::encode_id(email, &mut bytes).map_err(|_| Error::ServerPartialFail)?;
    output.format(format_args!(",\"updated\":null,\"notUpdated\":{{\"{id}\":{{\"type\":\"invalidProperties\",\"properties\":[\"mailboxIds\"]}}}},\"created\":null,\"notCreated\":null,\"destroyed\":null,\"notDestroyed\":null}}"))
        .map_err(|_| Error::ServerPartialFail)?;
    Ok("Email/set")
}
fn write_filing(
    output: &mut TextBuffer<'_>,
    result: Result<EmailUpdateCompletion, UploadError>,
    state: DataState,
    email: crate::ids::EmailId,
) -> Result<&'static str, Error> {
    if let Ok(complete) = result {
        if complete.before_epoch != state.epoch || complete.results.len() != 1 {
            return Err(Error::ServerPartialFail);
        }
        if let Ok(sequence) = complete.outcome {
            let old = DataState {
                epoch: complete.before_epoch,
                sequence: complete.before_sequence,
                kind: DataType::Email,
                ..state
            };
            states(output, old, DataState { sequence, ..old })?;
            let mut bytes = [0; wire::IdKind::WIRE_BYTES];
            let id = wire::encode_id(email, &mut bytes).map_err(|_| Error::ServerPartialFail)?;
            match complete.results.first().ok_or(Error::ServerPartialFail)? {
                EmailUpdateResult::Updated => output.format(format_args!(",\"updated\":{{\"{id}\":null}},\"notUpdated\":null")),
                EmailUpdateResult::NotFound => output.format(format_args!(",\"updated\":null,\"notUpdated\":{{\"{id}\":{{\"type\":\"notFound\"}}}}")),
                EmailUpdateResult::InvalidMailbox => output.format(format_args!(",\"updated\":null,\"notUpdated\":{{\"{id}\":{{\"type\":\"invalidProperties\",\"properties\":[\"mailboxIds\"]}}}}")),
            }.map_err(|_| Error::ServerPartialFail)?;
            output.append(",\"created\":null,\"notCreated\":null,\"destroyed\":null,\"notDestroyed\":null}").map_err(|_| Error::ServerPartialFail)?;
            return Ok("Email/set");
        }
        if matches!(complete.outcome, Err(CommitError::Indeterminate(_))) {
            output
                .append("{\"type\":\"serverPartialFail\"}")
                .map_err(|_| Error::ServerPartialFail)?;
            return Ok("error");
        }
    }
    output
        .append("{\"type\":\"serverUnavailable\"}")
        .map_err(|_| Error::ServerPartialFail)?;
    Ok("error")
}

#[cfg(test)]
#[path = "submission_set_tests.rs"]
mod tests;
