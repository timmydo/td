//! Ordered retained method batches; capability admission remains separate.
use super::{pointer, resolution, response, result_index, Invocation, CORE};
use crate::{ids::AccountId, ports, store_fs::ResponseRetention};
use std::{cmp::Ordering, ops::Range};
use td_json::{arena, arena_emit::BorrowedEmitter};
#[path = "email_set.rs"]
mod email_set;
pub use email_set::EmailMutator;
#[path = "email_create.rs"]
mod email_create;
pub use email_create::{DraftCreator, DraftError, DraftInput};
#[path = "mailbox_set.rs"]
mod mailbox_set;
pub use mailbox_set::MailboxMutator;

#[path = "email_query.rs"]
mod email_query;

#[path = "email_get.rs"]
mod email_get;
pub use email_get::{
    write_verified as write_verified_email, Output as EmailOutput, Selection as EmailProjection,
};
#[path = "get_ids.rs"]
mod get_ids;
#[path = "mailbox_get.rs"]
mod mailbox_get;
#[path = "mailbox_query.rs"]
mod mailbox_query;
#[path = "mailbox_query_args.rs"]
mod mailbox_query_args;
#[path = "mailbox_query_filter.rs"]
mod mailbox_query_filter;
#[path = "object_changes.rs"]
mod object_changes;
#[path = "push.rs"]
mod push;
#[path = "thread_get.rs"]
mod thread_get;
use super::{MAIL, SUBMISSION};
#[path = "identity_get.rs"]
mod identity_get;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    State,
    StateMismatch,
    ServerPartialFail,
    Storage(ports::Error),
    ExecutionDeadline,
    InvalidArguments,
    AccountNotFound,
    CannotCalculateChanges,
    UnsupportedFilter,
    UnsupportedSort,
    AnchorNotFound,
    FromAccountNotFound,
    InvalidResultReference,
    UnknownMethod,
    Forbidden,
    RequestTooLarge,
    ResponseCapacity,
    StorageCapacity,
    StorageBusy,
    MessageInterpretation,
}
impl std::fmt::Display for Error {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "JMAP dispatch failure: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Indexed complete arguments and their exclusively owned retention file.
/// Successful creation IDs and the captured Session state remain available for the
/// final envelope writer. No API/Session or Core capability is activated here.
pub struct Completed<'state, 'c, 's, 'r, 'sr, 'a, P> {
    responses: result_index::Responses,
    created: super::creation_ids::CreationIds,
    state: &'state str,
    output_started: bool,
    retention: ResponseRetention<'c, 's, 'r, 'sr, 'a, P>,
}
impl<'state, 'c, 's, 'r, 'sr, 'a, P> Completed<'state, 'c, 's, 'r, 'sr, 'a, P> {
    pub fn responses(&self) -> &result_index::Responses {
        &self.responses
    }
    pub fn session_state(&self) -> &'state str {
        self.state
    }
    pub fn into_request(self) -> super::OwnedRequest {
        self.responses.into_request()
    }
    pub fn encoder(
        &mut self,
    ) -> Result<super::wire::Encoder<'_, 'state, 'c, 's, 'r, 'sr, 'a, P>, super::wire::Error> {
        if self.output_started {
            return Err(super::wire::Error::State);
        }
        Ok(super::wire::Encoder::new(
            &self.responses,
            Some(&self.created),
            self.state,
            &mut self.retention,
            &mut self.output_started,
        ))
    }
}

/// Execute on a storage worker after authentication and whole-request
/// admission/preflight. `account` is the sole authenticated personal account,
/// supplied by trusted service configuration. The fresh file and index are
/// bound for this entire
/// invocation. All methods here are side-effect free; errors never stop later
/// calls. Terminal original work/I/O/outer-deadline failure aborts the batch.
/// Refusal drops the request arenas; allocation recycling is not promised.
/// Read-only fixtures use this entry; runtime dispatch requires a device.
#[cfg(test)]
pub fn run<'state, 'c, 's, 'r, 'sr, 'a, P>(
    plan: response::Plan<'state>,
    retention: ResponseRetention<'c, 's, 'r, 'sr, 'a, P>,
    execution: ports::Deadline,
    account: AccountId,
) -> Result<Completed<'state, 'c, 's, 'r, 'sr, 'a, P>, Error> {
    run_inner(plan, retention, execution, account, Profile::default())
}

/// Email-only mutation fixture; runtime uses the complete mail-access profile.
#[cfg(test)]
pub fn run_with_email_updates<'state, 'c, 's, 'r, 'sr, 'a, P>(
    plan: response::Plan<'state>,
    retention: ResponseRetention<'c, 's, 'r, 'sr, 'a, P>,
    execution: ports::Deadline,
    account: AccountId,
    device: crate::ids::DeviceId,
    flags: &dyn EmailMutator,
) -> Result<Completed<'state, 'c, 's, 'r, 'sr, 'a, P>, Error> {
    run_inner(
        plan,
        retention,
        execution,
        account,
        Profile {
            flags: Some((flags, device)),
            ..Profile::default()
        },
    )
}
/// Prepared singleton Email/set. Refuse mixed requests before any effect;
/// this path has no native read adapter or draft implementation.
pub fn run_with_postgres_updates<'state, 'c, 's, 'r, 'sr, 'a, P>(
    plan: response::Plan<'state>,
    retention: ResponseRetention<'c, 's, 'r, 'sr, 'a, P>,
    execution: ports::Deadline,
    account: AccountId,
    device: crate::ids::DeviceId,
    flags: &dyn EmailMutator,
) -> Result<Completed<'state, 'c, 's, 'r, 'sr, 'a, P>, Error> {
    let request = plan.request().map_err(|_| Error::State)?;
    if request.call_count() != 1
        || request
            .calls()
            .next()
            .is_none_or(|call| call.name != b"Email/set")
    {
        return Err(Error::InvalidArguments);
    }
    run_inner(
        plan,
        retention,
        execution,
        account,
        Profile {
            flags: Some((flags, device)),
            ..Profile::default()
        },
    )
}
/// Authenticated td-mail mutations, including root-folder creation.
pub fn run_with_mail_access<'state, 'c, 's, 'r, 'sr, 'a, P>(
    plan: response::Plan<'state>,
    retention: ResponseRetention<'c, 's, 'r, 'sr, 'a, P>,
    execution: ports::Deadline,
    account: AccountId,
    device: crate::ids::DeviceId,
    mutations: &(impl EmailMutator + MailboxMutator),
    drafts: Option<&dyn DraftCreator>,
) -> Result<Completed<'state, 'c, 's, 'r, 'sr, 'a, P>, Error> {
    run_inner(
        plan,
        retention,
        execution,
        account,
        Profile {
            flags: Some((mutations, device)),
            mailboxes: Some((mutations, device)),
            drafts: drafts.map(|creator| (creator, device)),
            identities: None,
            read: None,
            email_updates: false,
        },
    )
}
/// Prepared identity read fixture; runtime integration requires a device.
#[cfg(test)]
pub fn run_with_identities<'state, 'c, 's, 'r, 'sr, 'a, P>(
    plan: response::Plan<'state>,
    retention: ResponseRetention<'c, 's, 'r, 'sr, 'a, P>,
    execution: ports::Deadline,
    account: AccountId,
    identities: &crate::identity_catalog::Catalog,
) -> Result<Completed<'state, 'c, 's, 'r, 'sr, 'a, P>, Error> {
    run_inner(
        plan,
        retention,
        execution,
        account,
        Profile {
            identities: Some(identities),
            ..Profile::default()
        },
    )
}
/// Prepared PostgreSQL read dispatch. The caller owns the freshly authenticated
/// runtime snapshot and its original deadline for this batch. `clock` must be
/// the response owner's clock; its execution instant tightens SQL as well as
/// response work. Core, Mailbox/get, Email/get and Email/query are connected here;
/// production activation is a separate cutover.
pub fn run_with_postgres_read<'state, 'c, 's, 'r, 'sr, 'a, P>(
    plan: response::Plan<'state>,
    retention: ResponseRetention<'c, 's, 'r, 'sr, 'a, P>,
    execution: ports::Deadline,
    snapshot: &mut crate::postgres_read::Snapshot<'_>,
    clock: &crate::clock::RuntimeClock,
    email_updates: bool,
) -> Result<Completed<'state, 'c, 's, 'r, 'sr, 'a, P>, Error> {
    snapshot
        .constrain_deadline(
            clock
                .deadline_instant(execution)
                .map_err(mailbox_get::storage)?,
        )
        .map_err(postgres_read_error)?;
    let account = snapshot.identity().account;
    let complete = run_inner(
        plan,
        retention,
        execution,
        account,
        Profile {
            read: Some(snapshot),
            email_updates,
            ..Profile::default()
        },
    )?;
    snapshot.check().map_err(postgres_read_error)?;
    Ok(complete)
}

fn reborrow_read<'short, 'long: 'short>(
    view: &'short mut (dyn MailRead + 'long),
) -> &'short mut (dyn MailRead + 'short) {
    view
}

// This preparation seam disappears with the local database adapter at cutover.
trait MailRead: crate::email_response::Source<Error = crate::postgres_read::Error> {
    fn identity(&self) -> ports::ViewIdentity;
    fn check(&mut self) -> Result<(), Error>;
    fn mailboxes(&mut self) -> Result<Vec<crate::postgres_mailboxes::Mailbox>, Error>;
    fn query(
        &mut self,
        query: crate::postgres_read::EmailQuery,
    ) -> Result<crate::postgres_read::EmailPage, Error>;
    fn changes(
        &mut self,
        kind: crate::sync::DataType,
        since: crate::sync::DataState,
        maximum: usize,
        before_poll: &mut dyn FnMut() -> Result<(), ports::Error>,
    ) -> Result<crate::jmap_changes::Page, Error>;
    fn thread_emails(
        &mut self,
        thread: crate::ids::ThreadId,
        after: Option<ports::ThreadEmail>,
        maximum: usize,
    ) -> Result<Option<ports::ThreadEmails>, Error>;
    fn email_body(
        &mut self,
        email: crate::ids::EmailId,
        projection: email_get::BodyProjection<'_>,
        output: &mut dyn EmailOutput,
    ) -> Result<(), Error>;
}
impl MailRead for crate::postgres_read::Snapshot<'_> {
    fn identity(&self) -> ports::ViewIdentity {
        self.identity()
    }
    fn check(&mut self) -> Result<(), Error> {
        self.check().map_err(postgres_read_error)
    }
    fn mailboxes(&mut self) -> Result<Vec<crate::postgres_mailboxes::Mailbox>, Error> {
        self.mailbox_listing().map_err(postgres_read_error)
    }
    fn query(
        &mut self,
        query: crate::postgres_read::EmailQuery,
    ) -> Result<crate::postgres_read::EmailPage, Error> {
        self.query_emails(query).map_err(postgres_read_error)
    }
    fn changes(
        &mut self,
        kind: crate::sync::DataType,
        since: crate::sync::DataState,
        maximum: usize,
        before_poll: &mut dyn FnMut() -> Result<(), ports::Error>,
    ) -> Result<crate::jmap_changes::Page, Error> {
        self.changes(kind, since, maximum, before_poll)
            .map_err(postgres_read_error)
    }
    fn thread_emails(
        &mut self,
        thread: crate::ids::ThreadId,
        after: Option<ports::ThreadEmail>,
        maximum: usize,
    ) -> Result<Option<ports::ThreadEmails>, Error> {
        self.thread_emails(thread, after, maximum)
            .map_err(postgres_read_error)
    }
    fn email_body(
        &mut self,
        email: crate::ids::EmailId,
        projection: email_get::BodyProjection<'_>,
        output: &mut dyn EmailOutput,
    ) -> Result<(), Error> {
        let mut result = Err(Error::State);
        self.with_pinned_body(email, |metadata, pin| {
            if metadata.blob != projection.metadata.blob
                || metadata.body.length != projection.metadata.size
            {
                return Err(ports::Error::Corrupt);
            }
            result = write_verified_email(
                output,
                pin,
                projection.limits,
                (projection.selection, projection.options),
                projection.clock,
                projection.work,
            );
            Ok(())
        })
        .map_err(postgres_read_error)?;
        result
    }
}
fn postgres_read_error(error: crate::postgres_read::Error) -> Error {
    use crate::postgres_read::Error as Read;
    match error {
        Read::Connection(crate::postgres_probe::Error::Deadline)
        | Read::Mailboxes(crate::postgres_mailboxes::Error::Connection(
            crate::postgres_probe::Error::Deadline,
        ))
        | Read::Body(crate::postgres_body::Error::Connection(
            crate::postgres_probe::Error::Deadline,
        ))
        | Read::Body(crate::postgres_body::Error::Read(ports::Error::Deadline)) => {
            Error::ExecutionDeadline
        }
        Read::Busy => Error::StorageBusy,
        Read::Capacity
        | Read::Mailboxes(crate::postgres_mailboxes::Error::Capacity)
        | Read::Body(crate::postgres_body::Error::Capacity) => Error::StorageCapacity,
        Read::Output(error) => mailbox_get::storage(error),
        Read::Changes(error) => object_changes::changes(error),
        _ => Error::Storage(ports::Error::Invalid),
    }
}

fn run_inner<'state, 'c, 's, 'r, 'sr, 'a, P>(
    plan: response::Plan<'state>,
    mut retention: ResponseRetention<'c, 's, 'r, 'sr, 'a, P>,
    execution: ports::Deadline,
    account: AccountId,
    profile: Profile<'_>,
) -> Result<Completed<'state, 'c, 's, 'r, 'sr, 'a, P>, Error> {
    let Profile {
        flags,
        mailboxes,
        drafts,
        identities,
        read,
        email_updates,
    } = profile;
    if identities.is_some_and(|catalog| catalog.account() != account) {
        return Err(Error::State);
    }
    if !retention.is_empty() || plan.required_bytes() > retention.maximum() {
        return Err(Error::State);
    }
    // Validate the phase even for an empty batch. An expired valid phase
    // still permits per-method errors, or an empty response with no methods.
    let _ = retention.admit_before(execution).map_err(Error::Storage)?;
    let state = plan.session_state();
    let optional_end = retention
        .maximum()
        .checked_sub(plan.required_bytes())
        .ok_or(Error::State)?;
    let mut responses = result_index::Responses::new(plan.into_request());
    let request = responses.request().view().map_err(|_| Error::State)?;
    let mut core = false;
    let mut mail = false;
    let mut submission = false;
    for capability in request.using {
        retention.admit_work_turn().map_err(Error::Storage)?;
        core |= capability.as_string_bytes() == Some(CORE.as_bytes());
        mail |= capability.as_string_bytes() == Some(MAIL.as_bytes());
        submission |= capability.as_string_bytes() == Some(SUBMISSION.as_bytes());
    }
    let mut calls = Vec::new();
    calls
        .try_reserve_exact(request.call_count())
        .map_err(|_| Error::ResponseCapacity)?;
    for call in request.calls {
        retention.admit_work_turn().map_err(Error::Storage)?;
        calls.push(call.token_index());
    }
    let mut read = read;
    let mut worker = Worker {
        retention: &mut retention,
        execution,
        optional_end,
        created: Default::default(),
        account,
        flags,
        mailboxes,
        drafts,
        identities,
        read: read.as_mut().map(|view| reborrow_read(&mut **view)),
        email_updates,
        submission,
    };
    for (ordinal, token) in calls.into_iter().enumerate() {
        let result = worker.method(&responses, ordinal, token, core, mail);
        let (name, range) = match result {
            Ok(result) => result,
            Err(error) => {
                if let Some(error) = worker.retention.failure() {
                    return Err(Error::Storage(error));
                }
                let body: &[u8] = match error {
                    Error::StateMismatch => br#"{"type":"stateMismatch"}"#,
                    Error::ServerPartialFail => br#"{"type":"serverPartialFail"}"#,
                    Error::UnsupportedFilter => br#"{"type":"unsupportedFilter"}"#,
                    Error::UnsupportedSort => br#"{"type":"unsupportedSort"}"#,
                    Error::AnchorNotFound => br#"{"type":"anchorNotFound"}"#,
                    Error::CannotCalculateChanges => br#"{"type":"cannotCalculateChanges"}"#,
                    Error::AccountNotFound => br#"{"type":"accountNotFound"}"#,
                    Error::FromAccountNotFound => br#"{"type":"fromAccountNotFound"}"#,
                    Error::InvalidArguments => br#"{"type":"invalidArguments"}"#,
                    Error::InvalidResultReference => br#"{"type":"invalidResultReference"}"#,
                    Error::UnknownMethod => br#"{"type":"unknownMethod"}"#,
                    Error::Forbidden => br#"{"type":"forbidden"}"#,
                    Error::RequestTooLarge => br#"{"type":"requestTooLarge"}"#,
                    Error::ExecutionDeadline => {
                        br#"{"type":"serverUnavailable","description":"executionDeadline"}"#
                    }
                    Error::MessageInterpretation => {
                        br#"{"type":"serverFail","description":"messageInterpretation"}"#
                    }
                    Error::StorageCapacity => {
                        br#"{"type":"serverFail","description":"storageWorkLimit"}"#
                    }
                    Error::StorageBusy => {
                        br#"{"type":"serverUnavailable","description":"storageBusy"}"#
                    }
                    Error::ResponseCapacity => {
                        br#"{"type":"serverFail","description":"responseCapacity"}"#
                    }
                    Error::Storage(_) | Error::State => return Err(error),
                };
                worker.retention.rollback_tail().map_err(Error::Storage)?;
                let start = worker.retention.len();
                worker.retention.append(body).map_err(Error::Storage)?;
                let end = worker.retention.publish().map_err(Error::Storage)?;
                ("error", start..end)
            }
        };
        responses
            .append(
                ordinal,
                name,
                range.start,
                range.end.checked_sub(range.start).ok_or(Error::State)?,
                worker.retention.published_len(),
            )
            .map_err(|_| Error::State)?;
    }
    worker.retention.admit_work_turn().map_err(Error::Storage)?;
    let created = worker.created;
    Ok(Completed {
        responses,
        created,
        state,
        output_started: false,
        retention,
    })
}
#[derive(Default)]
struct Profile<'a> {
    flags: Option<(&'a dyn EmailMutator, crate::ids::DeviceId)>,
    mailboxes: Option<(&'a dyn MailboxMutator, crate::ids::DeviceId)>,
    drafts: Option<(&'a dyn DraftCreator, crate::ids::DeviceId)>,
    identities: Option<&'a crate::identity_catalog::Catalog>,
    read: Option<&'a mut dyn MailRead>,
    email_updates: bool,
}
struct Resolved {
    value: usize,
    range: Range<u64>,
}
struct Worker<'io, 'c, 's, 'r, 'sr, 'a, P> {
    retention: &'io mut ResponseRetention<'c, 's, 'r, 'sr, 'a, P>,
    execution: ports::Deadline,
    optional_end: u64,
    created: super::creation_ids::CreationIds,
    account: AccountId,
    flags: Option<(&'io dyn EmailMutator, crate::ids::DeviceId)>,
    mailboxes: Option<(&'io dyn MailboxMutator, crate::ids::DeviceId)>,
    drafts: Option<(&'io dyn DraftCreator, crate::ids::DeviceId)>,
    identities: Option<&'io crate::identity_catalog::Catalog>,
    read: Option<&'io mut dyn MailRead>,
    email_updates: bool,
    submission: bool,
}
impl<P> super::children::Source for Worker<'_, '_, '_, '_, '_, '_, P> {
    fn check(&mut self) -> Result<(), Error> {
        Worker::check(self)
    }
    fn read(&mut self, offset: u64, output: &mut [u8]) -> Result<(), Error> {
        self.retention
            .read_at(offset, output)
            .map_err(Error::Storage)
    }
}
impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    fn check(&mut self) -> Result<(), Error> {
        if !self
            .retention
            .admit_before(self.execution)
            .map_err(Error::Storage)?
        {
            return Err(Error::ExecutionDeadline);
        }
        Ok(())
    }
    fn prepare_creation(&mut self, key: &[u8]) -> Result<usize, Error> {
        self.check()?;
        let mut remaining = self.available()?;
        let slot = self.created.prepare(key, &mut remaining, || {
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
        self.optional_end = self
            .retention
            .len()
            .checked_add(remaining)
            .ok_or(Error::State)?;
        Ok(slot)
    }
    fn available(&self) -> Result<u64, Error> {
        self.optional_end
            .checked_sub(self.retention.len())
            .ok_or(Error::ResponseCapacity)
    }
    fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.check()?;
        if bytes.len() as u64 > self.available()? {
            return Err(Error::ResponseCapacity);
        }
        self.retention.append(bytes).map_err(Error::Storage)
    }
    fn compare(&mut self, left: &[u8], right: &[u8]) -> Result<Ordering, Error> {
        for (a, b) in left.chunks(256).zip(right.chunks(256)) {
            self.check()?;
            let order = a.cmp(b);
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(left.len().cmp(&right.len()))
    }
    fn collisions(&mut self, call: Invocation<'_>) -> Result<(), Error> {
        let mut references = call.arguments.as_object().ok_or(Error::State)?;
        let mut names = references;
        let mut name = names.next_bytes();
        while let Some((key, _)) = references.next_bytes() {
            self.check()?;
            let Some(normal) = key.strip_prefix(b"#") else {
                continue;
            };
            while let Some((key, _)) = name {
                self.check()?;
                match self.compare(key, normal)? {
                    Ordering::Less => name = names.next_bytes(),
                    Ordering::Equal => return Err(Error::InvalidArguments),
                    Ordering::Greater => break,
                }
            }
        }
        Ok(())
    }
    fn method(
        &mut self,
        responses: &result_index::Responses,
        ordinal: usize,
        token: usize,
        core: bool,
        mail: bool,
    ) -> Result<(&'static str, Range<u64>), Error> {
        self.check()?;
        let document = responses
            .request()
            .document
            .view()
            .map_err(|_| Error::State)?;
        let call = document
            .value_at(token)
            .and_then(super::invocation)
            .ok_or(Error::State)?;
        self.collisions(call)?;
        let mut resolved = Vec::new();
        let mut arguments = call.arguments.as_object().ok_or(Error::State)?;
        while let Some((key, value)) = arguments.next_bytes() {
            self.check()?;
            if key.first() != Some(&b'#') {
                continue;
            }
            resolved
                .try_reserve(1)
                .map_err(|_| Error::ResponseCapacity)?;
            let range = self.reference(responses, ordinal, value)?;
            resolved.push(Resolved {
                value: value.token_index(),
                range,
            });
        }
        if self.read.is_some()
            && !matches!(
                call.name,
                b"Core/echo"
                    | b"Mailbox/get"
                    | b"Email/get"
                    | b"Email/query"
                    | b"Thread/get"
                    | b"Mailbox/changes"
                    | b"Email/changes"
                    | b"Thread/changes"
            )
        {
            return Err(Error::UnknownMethod);
        }
        let name = match call.name {
            b"Identity/get" if self.submission && self.identities.is_some() => "Identity/get",
            b"Core/echo" if core => "Core/echo",
            b"Blob/copy" if core => "Blob/copy",
            b"PushSubscription/get" if core => "PushSubscription/get",
            b"PushSubscription/set" if core => "PushSubscription/set",
            b"Mailbox/query" if mail => "Mailbox/query",
            b"Mailbox/queryChanges" if mail => "Mailbox/queryChanges",
            b"Mailbox/get" if mail => "Mailbox/get",
            b"Mailbox/changes" if mail => "Mailbox/changes",
            b"Email/changes" if mail => "Email/changes",
            b"Email/query" if mail => "Email/query",

            b"Email/get" if mail => "Email/get",
            b"Email/set" if mail && self.flags.is_some() => "Email/set",
            b"Mailbox/set" if mail && self.mailboxes.is_some() => "Mailbox/set",
            b"Thread/changes" if mail => "Thread/changes",
            b"Thread/get" if mail => "Thread/get",
            _ => return Err(Error::UnknownMethod),
        };
        let start = self.retention.len();
        self.arguments(responses, call, &resolved)?;
        self.check()?;
        let end = self.retention.publish().map_err(Error::Storage)?;
        if name == "Core/echo" {
            return Ok((name, start..end));
        }
        let result = if name == "Identity/get" {
            self.identity_get(start..end, responses.request().envelope.maximum_objects)
        } else if name == "Mailbox/set" {
            self.mailbox_set(start..end, responses.request().envelope.maximum_objects)
        } else if name == "Email/set" {
            self.email_set(
                start..end,
                responses.request().envelope.maximum_objects,
                call.arguments,
            )
        } else if name == "Email/get" {
            self.email_get(start..end, &responses.request().envelope.mail_limits)
        } else if name == "Blob/copy" {
            self.blob_copy(start..end)
        } else if name == "Mailbox/changes" {
            self.object_changes(
                start..end,
                responses.request().envelope.maximum_objects,
                object_changes::ChangesKind::Mailbox,
            )
        } else if name == "Email/changes" {
            self.object_changes(
                start..end,
                responses.request().envelope.maximum_objects,
                object_changes::ChangesKind::Email,
            )
        } else if name == "Thread/changes" {
            self.object_changes(
                start..end,
                responses.request().envelope.maximum_objects,
                object_changes::ChangesKind::Thread,
            )
        } else if name == "Thread/get" {
            self.thread_get(start..end, responses.request().envelope.maximum_objects)
        } else if name == "Email/query" {
            self.email_query(start..end, responses.request().envelope.query_page)
        } else if name == "Mailbox/query" || name == "Mailbox/queryChanges" {
            let limits = &responses.request().envelope;
            self.mailbox_query(
                start..end,
                limits.query_page,
                limits.query_depth,
                limits.query_nodes,
                if name == "Mailbox/queryChanges" {
                    mailbox_query_args::Mode::Changes
                } else {
                    mailbox_query_args::Mode::Query
                },
            )
        } else if name == "Mailbox/get" {
            self.mailbox_get(start..end, responses.request().envelope.maximum_objects)
        } else {
            self.push_method(
                name,
                start..end,
                responses.request().envelope.maximum_objects,
            )
        }?;
        Ok((name, result))
    }
    fn reference(
        &mut self,
        responses: &result_index::Responses,
        ordinal: usize,
        value: arena::Value<'_>,
    ) -> Result<Range<u64>, Error> {
        let mut fields = value.as_object().ok_or(Error::InvalidResultReference)?;
        let (mut result_of, mut name, mut path) = (None, None, None);
        while let Some((key, value)) = fields.next_bytes() {
            self.check()?;
            match key {
                b"resultOf" => result_of = value.as_string_bytes(),
                b"name" => name = value.as_string_bytes(),
                b"path" => {
                    if let Some(bytes) = value.as_string_bytes() {
                        // Safe str conversion rescans once. Charge that bounded
                        // request-sized scan before invoking the str-based walker.
                        for _ in bytes.chunks(256) {
                            self.check()?;
                        }
                        path = value.as_str();
                        self.check()?;
                    }
                }
                _ => (),
            }
        }
        self.check()?;
        let result_of = result_of.ok_or(Error::InvalidResultReference)?;
        let name = name.ok_or(Error::InvalidResultReference)?;
        let path = path.ok_or(Error::InvalidResultReference)?;
        let mut lookup = responses
            .lookup(ordinal, result_of, name)
            .map_err(|_| Error::State)?;
        let span = loop {
            self.check()?;
            match lookup.step().map_err(|_| Error::State)? {
                result_index::Progress::Pending => (),
                result_index::Progress::Found(span) => break span,
                result_index::Progress::InvalidResultReference => {
                    return Err(Error::InvalidResultReference)
                }
            }
        };
        let end = span
            .offset()
            .checked_add(span.length())
            .ok_or(Error::State)?;
        let maximum = self.available()?;
        let mut resolution =
            resolution::Resolution::new(self.retention, span.offset()..end, path, maximum)
                .map_err(resolution_error)?;
        let mut scratch = [0; 4096];
        loop {
            // The resolution holds the same owner; its poll admits the original
            // deadline. The shorter phase is checked through that owner's clock.
            if !resolution
                .admit_before(self.execution)
                .map_err(resolution_error)?
            {
                resolution.cancel().map_err(resolution_error)?;
                return Err(Error::ExecutionDeadline);
            }
            if resolution.poll(&mut scratch).map_err(resolution_error)? {
                break;
            }
        }
        if !resolution
            .admit_before(self.execution)
            .map_err(resolution_error)?
        {
            resolution.cancel().map_err(resolution_error)?;
            return Err(Error::ExecutionDeadline);
        }
        resolution.finish().map_err(resolution_error)
    }
    fn arguments(
        &mut self,
        responses: &result_index::Responses,
        call: Invocation<'_>,
        resolved: &[Resolved],
    ) -> Result<(), Error> {
        self.append(b"{")?;
        let mut arguments = call.arguments.as_object().ok_or(Error::State)?;
        let mut first = true;
        let mut selected = resolved.iter();
        while let Some((key, value)) = arguments.next_bytes() {
            self.check()?;
            if !first {
                self.append(b",")?;
            }
            first = false;
            let hash = key.first() == Some(&b'#');
            let document = responses
                .request()
                .document
                .view()
                .map_err(|_| Error::State)?;
            let key_token = value.token_index().checked_sub(1).ok_or(Error::State)?;
            let emitter = BorrowedEmitter::member_name(
                document,
                key_token,
                usize::from(hash),
                self.available()?,
            )
            .map_err(emission_error)?;
            self.emit(emitter)?;
            self.append(b":")?;
            if hash {
                let selected = selected
                    .next()
                    .filter(|r| r.value == value.token_index())
                    .ok_or(Error::State)?;
                self.copy(selected.range.clone())?;
            } else {
                let document = responses
                    .request()
                    .document
                    .view()
                    .map_err(|_| Error::State)?;
                let emitter =
                    BorrowedEmitter::new(document, value.token_index(), self.available()?)
                        .map_err(emission_error)?;
                self.emit(emitter)?;
            }
        }
        if selected.next().is_some() {
            return Err(Error::State);
        }
        self.append(b"}")
    }
    fn emit(&mut self, mut emitter: BorrowedEmitter<'_>) -> Result<(), Error> {
        let mut scratch = [0; 4096];
        loop {
            self.check()?;
            let next = emitter.poll(&mut scratch).map_err(emission_error)?;
            self.append(scratch.get(..next.written).ok_or(Error::State)?)?;
            if next.complete {
                return Ok(());
            }
        }
    }
    fn copy(&mut self, range: Range<u64>) -> Result<(), Error> {
        let mut position = range.start;
        let mut scratch = [0; 4096];
        while position < range.end {
            self.check()?;
            let amount = (range.end - position).min(scratch.len() as u64) as usize;
            let chunk = scratch.get_mut(..amount).ok_or(Error::State)?;
            self.retention
                .read_at(position, chunk)
                .map_err(Error::Storage)?;
            self.append(chunk)?;
            position = position.checked_add(amount as u64).ok_or(Error::State)?;
        }
        Ok(())
    }
}
fn emission_error(error: td_json::arena_emit::Error) -> Error {
    match error {
        td_json::arena_emit::Error::Limit => Error::ResponseCapacity,
        td_json::arena_emit::Error::Invalid => Error::State,
    }
}
fn resolution_error(error: resolution::Error) -> Error {
    match error {
        resolution::Error::Pointer(pointer::Error::InvalidResultReference) => {
            Error::InvalidResultReference
        }
        resolution::Error::Store(ports::Error::Capacity) => Error::ResponseCapacity,
        resolution::Error::Store(error) => Error::Storage(error),
        resolution::Error::Pointer(_) | resolution::Error::State => Error::State,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
pub(super) mod tests {
    use super::*;
    use crate::{
        jmap_request::{OwnedRequest, OwnedValidation},
        limits::Limits,
        ports::{Deadline, Tick},
    };
    pub(crate) fn request(input: &str) -> OwnedRequest {
        request_with_limits(input, Limits::default())
    }
    fn request_with_limits(input: &str, limits: Limits) -> OwnedRequest {
        request_with_capabilities(input, limits, &[CORE])
    }
    pub(super) fn request_with_capabilities(
        input: &str,
        limits: Limits,
        capabilities: &[&str],
    ) -> OwnedRequest {
        let mut bytes = vec![0; limits.json_bytes].into_boxed_slice();
        bytes[..input.len()].copy_from_slice(input.as_bytes());
        let tokens = vec![arena::Token::EMPTY; limits.json_tokens].into_boxed_slice();
        let mut parser = arena::OwnedCursor::with_input_length(
            bytes,
            tokens,
            input.len(),
            arena::ParseLimits {
                max_bytes: limits.json_bytes,
                max_depth: limits.json_depth,
                max_tokens: limits.json_tokens,
            },
        )
        .unwrap();
        while !parser.step().unwrap() {}
        let mut validation = OwnedValidation::new(
            parser.finish().unwrap(),
            &limits.plan().unwrap(),
            capabilities,
        )
        .unwrap();
        while !validation.step().unwrap() {}
        validation.finish().unwrap()
    }
    fn execute(input: &str, phase: u64) -> Vec<(String, Vec<u8>, Vec<u8>)> {
        execute_control(input, phase, None, None).unwrap()
    }
    type Reply = (String, Vec<u8>, Vec<u8>);
    fn execute_control(
        input: &str,
        phase: u64,
        advance: Option<u64>,
        records: Option<u64>,
    ) -> Result<Vec<Reply>, Error> {
        execute_with_limits(input, phase, advance, records, Limits::default())
    }
    fn execute_with_limits(
        input: &str,
        phase: u64,
        advance: Option<u64>,
        records: Option<u64>,
        limits: Limits,
    ) -> Result<Vec<Reply>, Error> {
        let mut replies = Vec::new();
        let mut failure = None;
        resolution::tests::run(|coordinator, spool, _, clock| {
            let deadline = Deadline::after(Tick(0), 100).unwrap();
            let mut retention = coordinator.retain_response(spool, deadline).unwrap();
            let mut plan = response::Preflight::new(
                request_with_limits(input, limits),
                "session-1",
                retention.maximum(),
            )
            .unwrap();
            while !plan.step(&mut [0; 4096]).unwrap() {}
            if let Some(target) = advance {
                clock.advance_until(target);
            }
            if let Some(records) = records {
                retention.test_record_budget(records);
            }
            let mut complete = match run(
                plan.finish().unwrap(),
                retention,
                Deadline::after(Tick(0), phase).unwrap(),
                AccountId::from_bytes([1; 16]),
            ) {
                Ok(complete) => complete,
                Err(error) => {
                    failure = Some(error);
                    return;
                }
            };
            assert_eq!(complete.session_state(), "session-1");
            for i in 0..complete.responses().len() {
                let reply = complete.responses.get(i).unwrap().unwrap();
                let mut arguments = vec![0; reply.arguments.length() as usize];
                complete
                    .retention
                    .read_at(reply.arguments.offset(), &mut arguments)
                    .unwrap();
                replies.push((reply.name.into(), arguments, reply.call_id.to_vec()));
            }
            assert!(complete
                .responses()
                .get(complete.responses().len())
                .unwrap()
                .is_none());
            assert_eq!(complete.retention.len(), complete.retention.published_len());
        });
        match failure {
            Some(error) => Err(error),
            None => Ok(replies),
        }
    }
    fn batch(calls: &str) -> String {
        format!(r#"{{"using":["urn:ietf:params:jmap:core"],"methodCalls":{calls}}}"#)
    }
    #[test]
    fn postgres_deadline_classification_preserves_nested_mailbox_expiry() {
        assert_eq!(
            postgres_read_error(crate::postgres_read::Error::Mailboxes(
                crate::postgres_mailboxes::Error::Connection(
                    crate::postgres_probe::Error::Deadline
                ),
            )),
            Error::ExecutionDeadline,
        );
    }
    #[test]
    fn postgres_body_deadline_and_capacity_keep_their_classification() {
        for error in [
            crate::postgres_body::Error::Connection(crate::postgres_probe::Error::Deadline),
            crate::postgres_body::Error::Read(ports::Error::Deadline),
        ] {
            assert_eq!(
                postgres_read_error(crate::postgres_read::Error::Body(error)),
                Error::ExecutionDeadline
            );
        }
        assert_eq!(
            postgres_read_error(crate::postgres_read::Error::Body(
                crate::postgres_body::Error::Capacity
            )),
            Error::StorageCapacity
        );
    }
    #[test]
    fn core_dispatch_retains_responses_without_a_mail_database() {
        use crate::{
            admission::{DiskLimits, ViewMode, WorkLimits},
            ports::Clock,
            store_fs::{tests::Fixture, RequestSpool, ResponseCoordinator},
        };
        let files = Fixture::new();
        let mut root = files.locked();
        let clock = std::sync::Arc::new(crate::clock::RuntimeClock::new());
        let deadline = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
        let spool = RequestSpool::open(&mut root, clock.clone(), deadline).unwrap();
        let resources = Limits::default().plan().unwrap();
        let plan = DiskLimits::default()
            .plan(
                &resources,
                WorkLimits::default(),
                ViewMode::OnlineBackground,
            )
            .unwrap();
        let mut states = [const { crate::ownership::SlotState::EMPTY }; 4];
        let mut cells = [const { crate::admission::logical::Cell::EMPTY }; 4];
        let coordinator =
            ResponseCoordinator::new(spool, &plan, &mut states, &mut cells, clock).unwrap();
        let retention = coordinator.retain_response(deadline).unwrap();
        let mut preflight = response::Preflight::new(
            request(&batch(r#"[["Core/echo",{"value":42},"echo"]]"#)),
            "session",
            retention.maximum(),
        )
        .unwrap();
        while !preflight.step(&mut [0; 4096]).unwrap() {}
        let mut complete = run(
            preflight.finish().unwrap(),
            retention,
            Deadline::after(Tick(0), deadline.tick().0 - 1).unwrap(),
            AccountId::from_bytes([1; 16]),
        )
        .unwrap();
        let result = complete.responses.get(0).unwrap().unwrap();
        assert_eq!(result.name, "Core/echo");
        let mut bytes = vec![0; result.arguments.length() as usize];
        complete
            .retention
            .read_at(result.arguments.offset(), &mut bytes)
            .unwrap();
        assert_eq!(bytes, br#"{"value":42}"#);
        drop(complete);
        assert_eq!(coordinator.used().unwrap(), 0);
        assert!(!coordinator.admission_stopped());
    }
    #[test]
    fn echo_resolves_wildcards_and_first_duplicate_id_in_wire_order() {
        let replies = execute(
            &batch(
                r##"[
            ["Core/echo",{"list":[{"ids":["m1","m2"]},{"ids":["m3"]}]},"a"],
            ["Core/echo",{"#ids":{"resultOf":"a","name":"Core/echo","path":"/list/*/ids"}},"a"],
            ["Core/echo",{"#first":{"resultOf":"a","name":"Core/echo","path":"/list/0/ids/0"}},"c"]
        ]"##,
            ),
            90,
        );
        assert_eq!(replies.len(), 3);
        assert_eq!(
            replies[1],
            (
                "Core/echo".into(),
                br#"{"ids":["m1","m2","m3"]}"#.to_vec(),
                b"a".to_vec()
            )
        );
        assert_eq!(replies[2].1, br#"{"first":"m1"}"#);
    }
    #[test]
    fn errors_are_referenceable_and_later_calls_continue() {
        let replies = execute(
            &batch(
                r##"[
            ["No/such",{},"bad"],
            ["Core/echo",{"#x":{"resultOf":"bad","name":"error","path":"/type"}},"b"],
            ["Core/echo",{"x":0,"#x":{"resultOf":"bad","name":"error","path":"/type"}},"c"],
            ["Core/echo",{"#x":{"resultOf":1,"name":"error","path":"/type"}},"d"],
            ["Core/echo",{"ok":true},"e"],
            ["No/such",{"#x":{"resultOf":"missing","name":"error","path":"/type"}},"f"]
        ]"##,
            ),
            90,
        );
        assert_eq!(replies[0].1, br#"{"type":"unknownMethod"}"#);
        assert_eq!(replies[1].1, br#"{"x":"unknownMethod"}"#);
        assert_eq!(replies[2].1, br#"{"type":"invalidArguments"}"#);
        assert_eq!(replies[3].1, br#"{"type":"invalidResultReference"}"#);
        assert_eq!(replies[4].1, br#"{"ok":true}"#);
        assert_eq!(replies[5].1, br#"{"type":"invalidResultReference"}"#);
    }
    #[test]
    fn terminal_outer_deadline_and_record_exhaustion_abort_the_batch() {
        let input = batch(r#"[["Core/echo",{"x":"value"},"a"]]"#);
        assert_eq!(
            execute_control(&input, 90, Some(100), Some(1)),
            Err(Error::Storage(ports::Error::Capacity))
        );
        let input = batch(&format!(
            r#"[["Core/echo",{{"x":"{}"}},"a"]]"#,
            "x".repeat(50000)
        ));
        assert_eq!(
            execute_control(&input, 99, Some(100), None),
            Err(Error::Storage(ports::Error::Deadline))
        );
    }
    #[test]
    fn mid_method_phase_expiry_rolls_back_and_keeps_later_error_capacity() {
        let input = batch(&format!(
            r#"[["Core/echo",{{"x":"{}"}},"a"],["Core/echo",{{}},"b"]]"#,
            "x".repeat(50000)
        ));
        let replies = execute_control(&input, 50, Some(50), None).unwrap();
        assert_eq!(replies.len(), 2);
        for reply in replies {
            assert_eq!(reply.0, "error");
            assert_eq!(
                reply.1,
                br#"{"type":"serverUnavailable","description":"executionDeadline"}"#
            );
        }
    }
    #[test]
    fn sorted_collision_scan_handles_large_valid_reference_sets() {
        let arguments = (0..1600)
            .map(|i| format!(r##""#r{i:04}":{{"resultOf":"a","name":"Core/echo","path":"/v"}}"##))
            .collect::<Vec<_>>()
            .join(",");
        let input = batch(&format!(
            r#"[["Core/echo",{{"v":1}},"a"],["Core/echo",{{{arguments}}},"b"]]"#
        ));
        let replies = execute(&input, 90);
        assert_eq!(replies[1].0, "Core/echo");
        assert!(replies[1].1.starts_with(br#"{"r0000":1,"#));
        assert!(replies[1].1.ends_with(br#""r1599":1}"#));
    }
    #[test]
    fn collisions_precede_bad_references_independent_of_key_order() {
        let input = batch(r##"[["Core/echo",{"#a":{},"#z":{},"z":1},"x"]]"##);
        let replies = execute(&input, 90);
        assert_eq!(replies[0].0, "error");
        assert_eq!(replies[0].1, br#"{"type":"invalidArguments"}"#);
    }
    #[test]
    fn reference_phase_expiry_cancels_its_unpublished_tail() {
        resolution::tests::run(|coordinator, spool, _, clock| {
            let mut retention = coordinator
                .retain_response(spool, Deadline::after(Tick(0), 100).unwrap())
                .unwrap();
            let source = format!(r#"{{"x":[{}]}}"#, vec!["1"; 100].join(","));
            retention.append(source.as_bytes()).unwrap();
            retention.publish().unwrap();
            let input = batch(
                r##"[["Core/echo",{},"a"],["Core/echo",{"#x":{"resultOf":"a","name":"Core/echo","path":"/x/*"}},"b"]]"##,
            );
            let mut responses = result_index::Responses::new(request(&input));
            responses
                .append(
                    0,
                    "Core/echo",
                    0,
                    source.len() as u64,
                    retention.published_len(),
                )
                .unwrap();
            let view = responses.request().view().unwrap();
            let value = view.calls().nth(1).unwrap().argument("#x").unwrap();
            clock.advance_until(50);
            let mut worker = Worker {
                retention: &mut retention,
                execution: Deadline::after(Tick(0), 50).unwrap(),
                optional_end: 70000,
                created: Default::default(),
                account: AccountId::from_bytes([1; 16]),
                flags: None,
                mailboxes: None,
                drafts: None,
                identities: None,
                read: None,
                email_updates: false,
                submission: false,
            };
            assert_eq!(
                worker.reference(&responses, 1, value),
                Err(Error::ExecutionDeadline)
            );
            assert_eq!(retention.len(), source.len() as u64);
            assert_eq!(retention.published_len(), source.len() as u64);
            assert_eq!(retention.failure(), None);
        });
    }
    #[test]
    fn capacity_failure_preserves_room_for_errors_and_later_echo() {
        let calls = format!(
            r#"[["Core/echo",{{"large":"{}"}},"a"],["Core/echo",{{"ok":1}},"b"]]"#,
            "x".repeat(70000)
        );
        let replies = execute(&batch(&calls), 90);
        assert_eq!(
            replies[0].1,
            br#"{"type":"serverFail","description":"responseCapacity"}"#
        );
        assert_eq!(replies[1].1, br#"{"ok":1}"#);
    }
    #[test]
    fn expansion_capacity_after_a_completed_reference_preserves_later_calls() {
        let calls = format!(
            r##"[["Core/echo",{{"large":"{}","small":1}},"a"],["Core/echo",{{"#a":{{"resultOf":"a","name":"Core/echo","path":"/small"}},"#b":{{"resultOf":"a","name":"Core/echo","path":"/large"}}}},"b"],["Core/echo",{{"ok":1}},"c"]]"##,
            "x".repeat(40000)
        );
        let replies = execute(&batch(&calls), 90);
        assert_eq!(replies[0].0, "Core/echo");
        assert_eq!(replies[1].0, "error");
        assert_eq!(
            replies[1].1,
            br#"{"type":"serverFail","description":"responseCapacity"}"#
        );
        assert_eq!(replies[2].0, "Core/echo");
        assert_eq!(replies[2].1, br#"{"ok":1}"#);
    }
    #[test]
    fn phase_expiry_uses_outer_budget_to_retain_every_method_error() {
        let replies = execute(&batch(r#"[["Core/echo",{},"a"],["Core/echo",{},"b"]]"#), 1);
        assert_eq!(replies.len(), 2);
        assert!(replies
            .iter()
            .all(|r| r.1 == br#"{"type":"serverUnavailable","description":"executionDeadline"}"#));
    }
    #[test]
    fn absent_core_declaration_and_empty_batches_remain_distinct() {
        let replies = execute(r#"{"using":[],"methodCalls":[["Core/echo",{},"a"]]}"#, 90);
        assert_eq!(replies[0].1, br#"{"type":"unknownMethod"}"#);
        assert!(execute(r#"{"using":[],"methodCalls":[]}"#, 90).is_empty());
        assert!(execute(r#"{"using":[],"methodCalls":[]}"#, 1).is_empty());
        for phase in [100, 101] {
            assert_eq!(
                execute_control(r#"{"using":[],"methodCalls":[]}"#, phase, None, None),
                Err(Error::Storage(ports::Error::Invalid))
            );
        }
    }
    #[test]
    fn push_get_defaults_projection_and_absent_generic_ids() {
        let replies = execute(
            &batch(
                r#"[["PushSubscription/get",{},"a"],["PushSubscription/get",{"ids":["foreign","A_9-","foreign"],"properties":["id","types"]},"b"],["PushSubscription/get",{"ids":null,"properties":null},"c"]]"#,
            ),
            90,
        );
        assert_eq!(replies[0].0, "PushSubscription/get");
        assert_eq!(replies[0].1, br#"{"list":[],"notFound":[]}"#);
        assert_eq!(
            replies[1].1,
            br#"{"list":[],"notFound":["foreign","A_9-"]}"#
        );
        assert_eq!(replies[2].1, replies[0].1);
    }
    #[test]
    fn push_sensitive_projection_is_forbidden_even_with_no_ids() {
        for property in ["url", "keys"] {
            let calls = format!(
                r#"[["PushSubscription/get",{{"ids":[],"properties":["{property}"]}},"a"],["Core/echo",{{}},"b"]]"#
            );
            let replies = execute(&batch(&calls), 90);
            assert_eq!(replies[0].0, "error");
            assert_eq!(replies[0].1, br#"{"type":"forbidden"}"#);
            assert_eq!(replies[1].0, "Core/echo");
        }
    }
    #[test]
    fn push_set_refuses_creation_and_reports_absent_updates_and_destroys() {
        let replies = execute(
            &batch(
                r#"[["PushSubscription/set",{"create":{"c":{"url":"https://never-contact.invalid/","deviceClientId":"d"}},"update":{"foreign":{"expires":null}},"destroy":["gone","gone"]},"a"],["PushSubscription/set",{},"b"],["PushSubscription/set",{"create":{},"update":{},"destroy":[]},"c"]]"#,
            ),
            90,
        );
        assert_eq!(replies[0].0, "PushSubscription/set");
        assert_eq!(replies[0].1, br#"{"created":null,"updated":null,"destroyed":null,"notCreated":{"c":{"type":"forbidden"}},"notUpdated":{"foreign":{"type":"notFound"}},"notDestroyed":{"gone":{"type":"notFound"}}}"#);
        assert_eq!(replies[1].1, br#"{"created":null,"updated":null,"destroyed":null,"notCreated":null,"notUpdated":null,"notDestroyed":null}"#);
        assert_eq!(replies[1].1, replies[2].1);
    }
    #[test]
    fn push_argument_shapes_account_state_and_unknown_properties_are_invalid() {
        for (method, arguments) in [
            ("get", r#"{"accountId":"a"}"#),
            ("get", r#"{"ids":{}}"#),
            ("get", r#"{"ids":[""]}"#),
            ("get", r#"{"ids":["bad.id"]}"#),
            ("get", r#"{"properties":[true]}"#),
            ("get", r#"{"properties":["unknown"]}"#),
            ("set", r#"{"ifInState":null}"#),
            ("set", r#"{"accountId":"a"}"#),
            ("set", r#"{"create":{"a":false}}"#),
            ("set", r#"{"update":[]}"#),
            ("set", r#"{"destroy":{}}"#),
        ] {
            let calls = format!(
                r#"[["PushSubscription/{method}",{arguments},"a"],["Core/echo",{{}},"b"]]"#
            );
            let replies = execute(&batch(&calls), 90);
            assert_eq!(
                replies[0].1, br#"{"type":"invalidArguments"}"#,
                "{method}: {arguments}"
            );
            assert_eq!(replies[1].0, "Core/echo");
        }
    }
    #[test]
    fn push_resolves_arguments_before_validation_and_enforces_object_count() {
        let replies = execute(
            &batch(
                r##"[["Core/echo",{"ids":["a","b"],"sensitive":["keys"],"create":{"c":{}}},"seed"],["PushSubscription/get",{"#ids":{"resultOf":"seed","name":"Core/echo","path":"/ids"}},"get"],["PushSubscription/get",{"#properties":{"resultOf":"seed","name":"Core/echo","path":"/sensitive"}},"deny"],["PushSubscription/set",{"#create":{"resultOf":"seed","name":"Core/echo","path":"/create"}},"set"]]"##,
            ),
            90,
        );
        assert_eq!(replies[1].1, br#"{"list":[],"notFound":["a","b"]}"#);
        assert_eq!(replies[2].1, br#"{"type":"forbidden"}"#);
        assert_eq!(replies[3].0, "PushSubscription/set");
        assert_eq!(replies[3].1, br#"{"created":null,"updated":null,"destroyed":null,"notCreated":{"c":{"type":"forbidden"}},"notUpdated":null,"notDestroyed":null}"#);
        for count in [256, 257] {
            let ids = std::iter::repeat_n("\"a\"", count)
                .collect::<Vec<_>>()
                .join(",");
            let calls = format!(r#"[["PushSubscription/get",{{"ids":[{ids}]}},"a"]]"#);
            let replies = execute(&batch(&calls), 90);
            assert_eq!(
                replies[0].0,
                if count == 256 {
                    "PushSubscription/get"
                } else {
                    "error"
                }
            );
            if count == 257 {
                assert_eq!(replies[0].1, br#"{"type":"requestTooLarge"}"#);
            }
        }
    }
    #[test]
    fn push_object_limits_follow_the_admitted_configuration_and_sum_set_actions() {
        let limits = Limits {
            objects_per_method: 1,
            ..Limits::default()
        };
        for call in [
            r#"["PushSubscription/get",{"ids":["a","b"]},"a"]"#,
            r#"["PushSubscription/set",{"create":{"c":{}},"destroy":["a"]},"a"]"#,
            r#"["PushSubscription/set",{"update":{"a":{}},"destroy":["b"]},"a"]"#,
        ] {
            let replies =
                execute_with_limits(&batch(&format!("[{call}]")), 90, None, None, limits).unwrap();
            assert_eq!(replies[0].1, br#"{"type":"requestTooLarge"}"#);
        }
    }
    #[test]
    fn push_accepts_resolved_arguments_larger_than_the_request_arena() {
        let calls = format!(
            r##"[["Core/echo",{{"c":{{"url":"{}"}}}},"s"],["Core/echo",{{"#a":{{"resultOf":"s","name":"Core/echo","path":"/c"}},"#b":{{"resultOf":"s","name":"Core/echo","path":"/c"}},"#c":{{"resultOf":"s","name":"Core/echo","path":"/c"}},"#d":{{"resultOf":"s","name":"Core/echo","path":"/c"}}}},"wide"],["PushSubscription/set",{{"#create":{{"resultOf":"wide","name":"Core/echo","path":""}}}},"p"]]"##,
            "x".repeat(300)
        );
        let input = batch(&calls);
        assert!(input.len() < 1024);
        let replies = execute_with_limits(
            &input,
            90,
            None,
            None,
            Limits {
                json_bytes: 1024,
                ..Limits::default()
            },
        )
        .unwrap();
        assert!(replies[1].1.len() > 1024);
        assert_eq!(replies[2].0, "PushSubscription/set");
        assert_eq!(replies[2].1, br#"{"created":null,"updated":null,"destroyed":null,"notCreated":{"a":{"type":"forbidden"},"b":{"type":"forbidden"},"c":{"type":"forbidden"},"d":{"type":"forbidden"}},"notUpdated":null,"notDestroyed":null}"#);
    }
    #[test]
    fn push_scans_deep_reference_chains_and_split_utf8_without_retaining_subtrees() {
        let mut value = "{}".to_string();
        for _ in 0..56 {
            value = format!(r#"{{"nested":{value}}}"#);
        }
        let mut calls = vec![format!(r#"["Core/echo",{{"c":{value}}},"s0"]"#)];
        for n in 1..15 {
            calls.push(format!(r##"["Core/echo",{{"#c":{{"resultOf":"s{}","name":"Core/echo","path":""}}}},"s{n}"]"##, n-1));
        }
        calls.push(r##"["PushSubscription/set",{"#create":{"resultOf":"s14","name":"Core/echo","path":""}},"push"]"##.into());
        let replies = execute_with_limits(
            &batch(&format!("[{}]", calls.join(","))),
            90,
            None,
            None,
            Limits {
                json_depth: 64,
                json_methods: 16,
                ..Limits::default()
            },
        )
        .unwrap();
        assert_eq!(replies.len(), 16);
        assert_eq!(replies[15].0, "PushSubscription/set");
        assert!(std::str::from_utf8(&replies[15].1)
            .unwrap()
            .contains(r#""c":{"type":"forbidden"}"#));
        for padding in 4050..4100 {
            let call = format!(
                r#"[["PushSubscription/set",{{"create":{{"c":{{"{}é":"é"}}}}}},"p"]]"#,
                "x".repeat(padding)
            );
            let replies = execute(&batch(&call), 90);
            assert_eq!(replies[0].0, "PushSubscription/set");
        }
    }
    #[test]
    fn push_set_accepts_the_exact_aggregate_limit() {
        let calls = r#"[["PushSubscription/set",{"create":{"c":{}},"update":{"u":{}},"destroy":["d"]},"p"]]"#;
        let replies = execute_with_limits(
            &batch(calls),
            90,
            None,
            None,
            Limits {
                objects_per_method: 3,
                ..Limits::default()
            },
        )
        .unwrap();
        assert_eq!(replies[0].0, "PushSubscription/set");
    }
    #[test]
    fn push_output_capacity_rolls_back_provisional_results_and_runs_later_calls() {
        let ids = (0..165)
            .map(|n| format!("\"{n:06}{}\"", "x".repeat(194)))
            .collect::<Vec<_>>()
            .join(",");
        let input = batch(&format!(
            r#"[["PushSubscription/get",{{"ids":[{ids}]}},"p"],["Core/echo",{{"ok":true}},"after"]]"#
        ));
        let replies = execute(&input, 90);
        assert_eq!(
            replies[0].1,
            br#"{"type":"serverFail","description":"responseCapacity"}"#
        );
        assert_eq!(replies[1].1, br#"{"ok":true}"#);
    }
    fn copy_call(arguments: &str) -> String {
        batch(&format!(
            r#"[["Blob/copy",{arguments},"copy"],["Core/echo",{{"ok":true}},"next"]]"#
        ))
    }
    #[test]
    fn blob_copy_validates_single_account_pairs_and_preserves_later_calls() {
        let account = format!("a{}", "01".repeat(16));
        let other = format!("a{}", "02".repeat(16));
        for (source, destination, error) in [
            (other.as_str(), account.as_str(), "fromAccountNotFound"),
            (account.as_str(), other.as_str(), "accountNotFound"),
            (other.as_str(), other.as_str(), "accountNotFound"),
            (account.as_str(), account.as_str(), "invalidArguments"),
            ("foreign", account.as_str(), "fromAccountNotFound"),
            (account.as_str(), "foreign", "accountNotFound"),
            ("foreign", "foreign", "accountNotFound"),
        ] {
            for ids in ["[]", r#"["absent","absent"]"#] {
                let arguments = format!(
                    r#"{{"fromAccountId":"{source}","accountId":"{destination}","blobIds":{ids}}}"#
                );
                let replies = execute(&copy_call(&arguments), 90);
                assert_eq!(
                    replies[0],
                    (
                        "error".into(),
                        format!(r#"{{"type":"{error}"}}"#).into_bytes(),
                        b"copy".to_vec()
                    )
                );
                assert_eq!(replies[1].1, br#"{"ok":true}"#);
            }
        }
    }
    #[test]
    fn blob_copy_checks_all_arguments_before_account_errors() {
        for arguments in [
            r#"{}"#,
            r#"{"accountId":"foreign","blobIds":[]}"#,
            r#"{"fromAccountId":"foreign","blobIds":[]}"#,
            r#"{"fromAccountId":"foreign","accountId":"foreign"}"#,
            r#"{"fromAccountId":null,"accountId":"foreign","blobIds":[]}"#,
            r#"{"fromAccountId":"foreign","accountId":1,"blobIds":[]}"#,
            r#"{"fromAccountId":"foreign","accountId":"foreign","blobIds":null}"#,
            r#"{"fromAccountId":"foreign","accountId":"foreign","blobIds":{}}"#,
            r#"{"fromAccountId":"foreign","accountId":"foreign","blobIds":[null]}"#,
            r#"{"fromAccountId":"foreign","accountId":"foreign","blobIds":["bad id"]}"#,
            r#"{"fromAccountId":"bad id","accountId":"foreign","blobIds":[]}"#,
            r#"{"fromAccountId":"foreign","accountId":"foreign","blobIds":[],"extra":true}"#,
        ] {
            let replies = execute(&copy_call(arguments), 90);
            assert_eq!(
                replies[0].1, br#"{"type":"invalidArguments"}"#,
                "{arguments}"
            );
            assert_eq!(replies[1].1, br#"{"ok":true}"#);
        }
        let limits = Limits {
            objects_per_method: 1,
            ..Limits::default()
        };
        let input = copy_call(
            r#"{"fromAccountId":"foreign","accountId":"foreign","blobIds":["same","same"]}"#,
        );
        for input in [
            input.clone(),
            input.replace("\"same\",\"same\"", "\"same\""),
        ] {
            let replies = execute_with_limits(&input, 90, None, None, limits).unwrap();
            assert_eq!(replies[0].1, br#"{"type":"accountNotFound"}"#);
            assert_eq!(replies[1].1, br#"{"ok":true}"#);
        }
        for value in ["".to_owned(), "bad id".to_owned(), "x".repeat(256)] {
            for field in ["fromAccountId", "accountId", "blobIds"] {
                let source = if field == "fromAccountId" {
                    &value
                } else {
                    "foreign"
                };
                let destination = if field == "accountId" {
                    &value
                } else {
                    "foreign"
                };
                let blob = if field == "blobIds" { &value } else { "blob" };
                let arguments = format!(
                    r#"{{"fromAccountId":"{source}","accountId":"{destination}","blobIds":["{blob}"]}}"#
                );
                assert_eq!(
                    execute(&copy_call(&arguments), 90)[0].1,
                    br#"{"type":"invalidArguments"}"#
                );
            }
        }
    }
    #[test]
    fn blob_copy_uses_resolved_arguments_core_opt_in_and_phase_deadline() {
        let account = format!("a{}", "01".repeat(16));
        let input = batch(&format!(
            r##"[
            ["Core/echo",{{"source":"foreign","blobs":["blob"]}},"first"],
            ["Blob/copy",{{"accountId":"{account}",
                "#fromAccountId":{{"resultOf":"first","name":"Core/echo","path":"/source"}},
                "#blobIds":{{"resultOf":"first","name":"Core/echo","path":"/blobs"}}}},"copy"],
            ["Core/echo",{{"#error":{{"resultOf":"copy","name":"error","path":"/type"}}}},"last"]
        ]"##
        ));
        let replies = execute(&input, 90);
        assert_eq!(replies[1].1, br#"{"type":"fromAccountNotFound"}"#);
        assert_eq!(replies[2].1, br#"{"error":"fromAccountNotFound"}"#);
        let input = copy_call(r#"{"fromAccountId":"foreign","accountId":"foreign","blobIds":[]}"#);
        let replies = execute(&input.replace(r#"["urn:ietf:params:jmap:core"]"#, "[]"), 90);
        assert_eq!(replies[0].1, br#"{"type":"unknownMethod"}"#);
        let replies = execute(&input, 0);
        assert_eq!(
            replies[0].1,
            br#"{"type":"serverUnavailable","description":"executionDeadline"}"#
        );
    }
    #[test]
    fn blob_copy_walk_checks_execution_during_retained_array_scan() {
        resolution::tests::run(|coordinator, spool, _, clock| {
            let until = Deadline::after(Tick(0), 100).unwrap();
            let mut retention = coordinator.retain_response(spool, until).unwrap();
            let ids = vec!["\"blob\""; 256].join(",");
            let arguments =
                format!(r#"{{"fromAccountId":"foreign","accountId":"foreign","blobIds":[{ids}]}}"#);
            retention.append(arguments.as_bytes()).unwrap();
            let end = retention.publish().unwrap();
            clock.advance_until(50);
            let mut worker = Worker {
                retention: &mut retention,
                execution: Deadline::after(Tick(0), 50).unwrap(),
                optional_end: 70000,
                created: Default::default(),
                account: AccountId::from_bytes([1; 16]),
                flags: None,
                mailboxes: None,
                drafts: None,
                identities: None,
                read: None,
                email_updates: false,
                submission: false,
            };
            assert_eq!(worker.blob_copy(0..end), Err(Error::ExecutionDeadline));
            assert_eq!(retention.failure(), None);
            assert_eq!(retention.published_len(), end);
        });
    }
}
