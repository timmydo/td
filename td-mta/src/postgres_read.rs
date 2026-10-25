//! Email metadata, values and verified bodies under one authenticated snapshot.
use crate::{
    device_credentials::Presented,
    format::key::Key,
    ids::{DeviceId, EmailId, MailboxId, ThreadId},
    ports::{self, BlobReader, ViewIdentity},
    postgres_body::{self, BlobMetadata, Metadata, ReadRequest},
    postgres_devices, postgres_mailboxes,
    postgres_probe::{self, Request, Worker},
    postgres_runtime_profile::Binding,
    postgres_schema, postgres_snapshot,
};
use std::time::Instant;
use td_crypto::TrustStore;
use tokio_postgres::types::Type;

#[path = "postgres_read_values.rs"]
mod value_reads;

#[path = "postgres_read_submissions.rs"]
mod submission_reads;
pub use submission_reads::{
    SubmissionMetadata, SubmissionPage, SubmissionQuery, MAX_SUBMISSION_PAGE,
};

pub use crate::email_response::{EmailValues as Values, MAX_EMAIL_VALUES as MAX_VALUES};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Resolution(crate::postgres_resolver::Error),
    Schema(postgres_schema::Error),
    Credential(postgres_devices::Error),
    Epoch,
    Policy,
    Unauthenticated,
    Capacity,
    Busy,
    Integrity,
    Database,
    Invalid,
    Body(postgres_body::Error),
    Mailboxes(postgres_mailboxes::Error),
    Output(ports::Error),
    Changes(crate::jmap_changes::Error),
}
impl From<ports::Error> for Error {
    fn from(error: ports::Error) -> Self {
        Self::Output(error)
    }
}

/// The mailbox filter shapes already supported by td-mail's Email/query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmailScope {
    All,
    Mailbox(MailboxId),
    Empty,
}
#[derive(Clone, Copy, Debug)]
pub struct EmailQuery {
    pub scope: EmailScope,
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub ascending: bool,
    pub position: u64,
    pub limit: usize,
    pub calculate_total: bool,
}
#[derive(Debug)]
pub struct EmailPage {
    pub state: crate::sync::QueryState,
    pub position: u64,
    pub ids: Vec<EmailId>,
    pub total: Option<u64>,
    pub has_more: bool,
}
pub const MAX_EMAIL_PAGE: usize = 1024;

/// Fixed process-local read connection capacity. No waiter queue or retry.
/// Reserve independent capacity for health and settlement outside this pool.
pub struct ReadSlots {
    maximum: usize,
    used: std::sync::atomic::AtomicUsize,
}
impl ReadSlots {
    pub fn new(maximum: usize) -> Result<Self, Error> {
        if !(1..=256).contains(&maximum) {
            return Err(Error::Invalid);
        }
        Ok(Self {
            maximum,
            used: std::sync::atomic::AtomicUsize::new(0),
        })
    }
    pub(crate) fn acquire(&self) -> Result<ReadPermit<'_>, Error> {
        use std::sync::atomic::Ordering;
        let mut used = self.used.load(Ordering::Relaxed);
        for _ in 0..8 {
            if used >= self.maximum {
                return Err(Error::Busy);
            }
            match self.used.compare_exchange_weak(
                used,
                used + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(ReadPermit(self)),
                Err(current) => used = current,
            }
        }
        Err(Error::Busy)
    }
}
pub(crate) struct ReadPermit<'a>(&'a ReadSlots);
impl Drop for ReadPermit<'_> {
    fn drop(&mut self) {
        self.0
            .used
            .fetch_sub(1, std::sync::atomic::Ordering::Release);
    }
}

type MetadataWindow = [Option<(EmailId, Option<Metadata>)>; postgres_body::MAX_METADATA_IDS];
const _: () = assert!(
    std::mem::size_of::<MetadataWindow>() + std::mem::size_of::<Option<Metadata>>() <= 5 * 1024
);

/// Owns one authenticated snapshot and its original deadline and budgets.
/// Worker-local: its poll waker is bound to the thread that opened it.
/// The connection drops before its reserved slot becomes available again.
pub struct Snapshot<'a> {
    pub(crate) header_cache: Option<&'a mut crate::postgres_header_cache::Cache>,
    worker: SnapshotWorker<'a>,
    identity: ViewIdentity,
    binding: Binding,
    device: DeviceId,
    deadline: Instant,
    failed: Option<Error>,
    metadata: Option<Metadata>,
    metadata_window: MetadataWindow,
    value_window: value_reads::Window,
    _worker_thread: std::marker::PhantomData<std::rc::Rc<()>>,
    _permit: Option<ReadPermit<'a>>,
}

enum SnapshotWorker<'a> {
    Owned(Worker),
    Borrowed(&'a mut Worker),
}
impl std::ops::Deref for SnapshotWorker<'_> {
    type Target = Worker;
    fn deref(&self) -> &Worker {
        match self {
            Self::Owned(worker) => worker,
            Self::Borrowed(worker) => worker,
        }
    }
}
impl std::ops::DerefMut for SnapshotWorker<'_> {
    fn deref_mut(&mut self) -> &mut Worker {
        match self {
            Self::Owned(worker) => worker,
            Self::Borrowed(worker) => worker,
        }
    }
}

impl crate::email_response::Source for Snapshot<'_> {
    type Error = Error;
    fn check_snapshot(&mut self) -> Result<(), Error> {
        self.check()
    }
    fn email_metadata(
        &mut self,
        email: EmailId,
        _scratch: &mut [u8],
    ) -> Result<Option<crate::email_response::EmailMetadata>, Error> {
        self.email(email).map(|metadata| {
            metadata.map(|metadata| crate::email_response::EmailMetadata {
                identity: metadata.identity,
                email: metadata.email,
                blob: metadata.blob,
                thread: metadata.thread,
                size: metadata.body.length,
                received_at: metadata.received_at,
            })
        })
    }
    fn email_mailboxes(
        &mut self,
        email: EmailId,
        after: Option<MailboxId>,
        maximum: usize,
    ) -> Result<Values<MailboxId>, Error> {
        self.mailboxes(email, after, maximum)
    }
    fn email_keywords(
        &mut self,
        email: EmailId,
        after: Option<&str>,
        maximum: usize,
    ) -> Result<Values<String>, Error> {
        self.keywords(email, after, maximum)
    }
}

pub fn with_snapshot<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    binding: &Binding,
    password: &[u8],
    read: impl FnOnce(&mut Snapshot<'_>) -> Result<T, Error>,
) -> Result<T, Error> {
    // Existing preparation callers already reserve their worker externally.
    let slots = ReadSlots::new(1)?;
    let mut view = Snapshot::open(&slots, request, trust, deadline, binding, password)?;
    view.check()?;
    let result = read(&mut view);
    view.check()?;
    result
}
impl<'a> Snapshot<'a> {
    /// Admit only qualified runtime connections, after reserving a shared slot.
    /// The selected profile remains passive until the snapshot authenticates it.
    pub fn open_runtime(
        slots: &'a ReadSlots,
        request: &Request<'_>,
        trust: &TrustStore,
        deadline: Instant,
        binding: &Binding,
        password: &[u8],
    ) -> Result<Self, Error> {
        if request.mode != postgres_probe::Mode::Runtime {
            return Err(Error::Invalid);
        }
        Self::open(slots, request, trust, deadline, binding, password)
    }
    /// Fresh DNS and primary snapshot admission share one reserved slot and
    /// original allowance. Only transport connection failures try another
    /// resolved address; policy, authentication and database failures refuse.
    pub fn open_resolved(
        slots: &'a ReadSlots,
        loaded: &crate::postgres_input::Loaded,
        executable: &std::path::Path,
        deadline: Instant,
        binding: &Binding,
        password: &[u8],
    ) -> Result<Self, Error> {
        if !executable.is_absolute() || loaded.bootstrap.profile() != binding.profile() {
            return Err(Error::Invalid);
        }
        if !binding.roles().jmap() {
            return Err(Error::Policy);
        }
        let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
        let now = Instant::now();
        if now >= deadline {
            return Err(Error::Connection(postgres_probe::Error::Deadline));
        }
        let deadline = deadline.min(
            now.checked_add(postgres_probe::MAX_DURATION)
                .ok_or(Error::Invalid)?,
        );
        BetweenMethods {
            permit: slots.acquire()?,
            binding: binding.clone(),
            device: presented.id(),
            deadline,
        }
        .reopen(loaded, executable, password)
    }
    fn open(
        slots: &'a ReadSlots,
        request: &Request<'_>,
        trust: &TrustStore,
        deadline: Instant,
        binding: &Binding,
        password: &[u8],
    ) -> Result<Self, Error> {
        if !binding.roles().jmap() {
            return Err(Error::Policy);
        }
        let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
        let device = presented.id();
        let deadline = deadline.min(
            Instant::now()
                .checked_add(postgres_probe::MAX_DURATION)
                .ok_or(Error::Invalid)?,
        );
        let permit = slots.acquire()?;
        let (worker, identity) =
            connect_authenticated(request, trust, deadline, binding, presented)?;
        Self::from_worker(permit, worker, identity, deadline, binding, device)
    }
    fn from_worker(
        permit: ReadPermit<'a>,
        worker: Worker,
        identity: ViewIdentity,
        deadline: Instant,
        binding: &Binding,
        device: DeviceId,
    ) -> Result<Self, Error> {
        Self::from_lease(
            Some(permit),
            SnapshotWorker::Owned(worker),
            identity,
            deadline,
            binding,
            device,
        )
    }
    pub(crate) fn borrowed(
        worker: &'a mut Worker,
        identity: ViewIdentity,
        deadline: Instant,
        binding: &Binding,
        device: DeviceId,
    ) -> Result<Self, Error> {
        Self::from_lease(
            None,
            SnapshotWorker::Borrowed(worker),
            identity,
            deadline,
            binding,
            device,
        )
    }
    fn from_lease(
        permit: Option<ReadPermit<'a>>,
        worker: SnapshotWorker<'a>,
        identity: ViewIdentity,
        deadline: Instant,
        binding: &Binding,
        device: DeviceId,
    ) -> Result<Self, Error> {
        let mut view = Self {
            header_cache: None,
            worker,
            identity,
            binding: binding.clone(),
            device,
            deadline,
            failed: None,
            metadata: None,
            metadata_window: [None; postgres_body::MAX_METADATA_IDS],
            value_window: value_reads::Window::default(),
            _worker_thread: std::marker::PhantomData,
            _permit: permit,
        };
        view.check()?;
        Ok(view)
    }
}
fn connect_authenticated(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    binding: &Binding,
    presented: Presented,
) -> Result<(Worker, ViewIdentity), Error> {
    let binding = binding.clone();
    let mut worker =
        postgres_probe::body_worker(request, trust, deadline).map_err(Error::Connection)?;
    let identity = worker
        .run(|client| {
            Box::pin(async move {
                postgres_snapshot::begin(client, &binding, presented)
                    .await
                    .map_err(snapshot_error)
            })
        })
        .map_err(Error::Connection)??;
    Ok((worker, identity))
}

/// A reserved read connection slot between methods, with no live socket or
/// transaction. Reopening never renews the original deadline or authority.
pub struct BetweenMethods<'a> {
    permit: ReadPermit<'a>,
    binding: Binding,
    device: DeviceId,
    deadline: Instant,
}
impl<'a> Snapshot<'a> {
    /// Validate and close this snapshot before admitting a separate mutation.
    /// No body pin can outlive this consuming transition.
    pub fn finish_method(mut self) -> Result<BetweenMethods<'a>, Error> {
        self.check()?;
        let Self {
            worker,
            binding,
            device,
            deadline,
            _permit: permit,
            ..
        } = self;
        let permit = permit.ok_or(Error::Invalid)?;
        drop(worker);
        Ok(BetweenMethods {
            permit,
            binding,
            device,
            deadline,
        })
    }
}
impl<'a> BetweenMethods<'a> {
    /// Fresh DNS, runtime-role qualification and full credential authentication
    /// on the original profile/account binding and reserved capacity.
    pub fn reopen(
        self,
        loaded: &crate::postgres_input::Loaded,
        executable: &std::path::Path,
        password: &[u8],
    ) -> Result<Snapshot<'a>, Error> {
        if !executable.is_absolute() || loaded.bootstrap.profile() != self.binding.profile() {
            return Err(Error::Invalid);
        }
        let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
        if presented.id() != self.device {
            return Err(Error::Unauthenticated);
        }
        let now = Instant::now();
        if now >= self.deadline {
            return Err(Error::Connection(postgres_probe::Error::Deadline));
        }
        let dns_deadline = self.deadline.min(
            now.checked_add(std::time::Duration::from_secs(5))
                .ok_or(Error::Invalid)?,
        );
        let addresses =
            crate::postgres_resolver::resolve(executable, &loaded.bootstrap, dns_deadline)
                .map_err(|error| match error {
                    crate::postgres_resolver::Error::Deadline => {
                        Error::Connection(postgres_probe::Error::Deadline)
                    }
                    error => Error::Resolution(error),
                })?;
        for address in addresses {
            let mut request = loaded.bootstrap.request(address);
            request.mode = postgres_probe::Mode::Runtime;
            let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
            let result = connect_authenticated(
                &request,
                &loaded.trust,
                self.deadline,
                &self.binding,
                presented,
            );
            if Instant::now() >= self.deadline {
                return Err(Error::Connection(postgres_probe::Error::Deadline));
            }
            match result {
                Ok((worker, identity)) => {
                    return Snapshot::from_worker(
                        self.permit,
                        worker,
                        identity,
                        self.deadline,
                        &self.binding,
                        self.device,
                    )
                }
                Err(Error::Connection(postgres_probe::Error::Transport)) => (),
                Err(error) => return Err(error),
            }
        }
        Err(Error::Connection(postgres_probe::Error::Transport))
    }
}

impl Snapshot<'_> {
    /// Tighten the original allowance for a shorter enclosing request phase.
    /// Never renews time, including after a terminal failure.
    pub fn constrain_deadline(&mut self, deadline: Instant) -> Result<(), Error> {
        self.deadline = self.deadline.min(deadline);
        let result = self
            .worker
            .constrain_deadline(self.deadline)
            .map_err(Error::Connection);
        self.retain(result)
    }
    pub fn identity(&self) -> ViewIdentity {
        self.identity
    }
    pub fn identity_catalog(&self) -> Option<std::sync::Arc<crate::identity_catalog::Catalog>> {
        self.binding.identity_catalog()
    }
    pub fn check(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if let Err(error) = self.worker.check().map_err(Error::Connection) {
            self.failed = Some(error);
            return Err(error);
        }
        Ok(())
    }
    fn retain<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failed = Some(error);
        }
        self.check()?;
        result
    }
    /// Fold a transaction-aligned page on this snapshot, charging each history
    /// poll to the caller's original request allowance.
    pub fn changes(
        &mut self,
        kind: crate::sync::DataType,
        since: crate::sync::DataState,
        maximum: usize,
        before_poll: &mut dyn FnMut() -> Result<(), ports::Error>,
    ) -> Result<crate::jmap_changes::Page, Error> {
        self.check()?;
        let result = crate::postgres_changes::read_snapshot(
            &mut self.worker,
            self.identity,
            kind,
            since,
            maximum,
            before_poll,
        )
        .map_err(Error::Changes);
        if matches!(
            result,
            Err(Error::Changes(
                crate::jmap_changes::Error::CannotCalculateChanges
                    | crate::jmap_changes::Error::Invalid
            ))
        ) {
            self.check()?;
            result
        } else {
            self.retain(result)
        }
    }
    /// Names, hierarchy and counts share this view's policy/device admission,
    /// state and deadline. Returned rows confer no later read authority.
    pub fn mailbox_listing(&mut self) -> Result<Vec<postgres_mailboxes::Mailbox>, Error> {
        self.check()?;
        let identity = self.identity;
        let deadline = self.deadline;
        let result = self
            .worker
            .run(|client| Box::pin(postgres_mailboxes::read(client, identity, deadline)))
            .map_err(Error::Connection)
            .and_then(|result| result.map_err(Error::Mailboxes));
        self.retain(result)
    }
    /// Prefetch only passive metadata inside this snapshot; missing IDs are
    /// retained explicitly. Complete validation precedes window replacement.
    pub fn prefetch_metadata(&mut self, emails: &[EmailId]) -> Result<(), Error> {
        self.check()?;
        let result = (|| {
            if emails.len() > postgres_body::MAX_METADATA_IDS {
                return Err(Error::Capacity);
            }
            let mut window = [None; postgres_body::MAX_METADATA_IDS];
            if emails.is_empty() {
                return Ok(window);
            }
            let mut ids = Vec::new();
            ids.try_reserve_exact(emails.len())
                .map_err(|_| Error::Capacity)?;
            ids.extend_from_slice(emails);
            let identity = self.identity;
            let rows = self
                .worker
                .run(|client| {
                    Box::pin(async move {
                        postgres_body::email_metadata_batch(
                            client,
                            identity.epoch,
                            identity.account,
                            postgres_body::MAX_BODY_BYTES,
                            &ids,
                        )
                        .await
                    })
                })
                .map_err(Error::Connection)?
                .map_err(Error::Body)?;
            if rows.iter().any(|row| row.identity != identity) {
                return Err(Error::Integrity);
            }
            for (index, email) in emails.iter().copied().enumerate() {
                let value = rows.iter().find(|row| row.email == email).copied();
                *window.get_mut(index).ok_or(Error::Integrity)? = Some((email, value));
            }
            Ok(window)
        })();
        let window = self.retain(result)?;
        self.metadata_window = window;
        self.value_window = value_reads::Window::default();
        self.metadata = None;
        Ok(())
    }
    /// Prefetch bounded value prefixes; pagination continues after their tail.
    pub fn prefetch_values(
        &mut self,
        emails: &[EmailId],
        mailboxes: bool,
        keywords: bool,
    ) -> Result<(), Error> {
        self.check()?;
        if emails.len() > postgres_body::MAX_METADATA_IDS {
            return self.retain(Err(Error::Capacity));
        }
        if !mailboxes && !keywords {
            self.value_window = value_reads::Window::default();
            return Ok(());
        }
        let mut ids = Vec::new();
        let reserve = ids
            .try_reserve_exact(emails.len())
            .map_err(|_| Error::Capacity);
        self.retain(reserve)?;
        ids.extend_from_slice(emails);
        let identity = self.identity;
        let result = self
            .worker
            .run(|client| {
                Box::pin(async move {
                    value_reads::Window::read(client, identity, &ids, mailboxes, keywords).await
                })
            })
            .map_err(Error::Connection)
            .and_then(|result| result);
        self.value_window = self.retain(result)?;
        Ok(())
    }
    /// Metadata alone does not authorize body output; with_body verifies the
    /// exact same snapshot's chunks before calling its consumer.
    pub fn email(&mut self, email: EmailId) -> Result<Option<Metadata>, Error> {
        self.check()?;
        // Stored fields and MIME projection share this immutable snapshot.
        if let Some((_, metadata)) = self
            .metadata_window
            .iter()
            .flatten()
            .find(|(id, _)| *id == email)
        {
            return Ok(*metadata);
        }
        if let Some(metadata) = self.metadata.filter(|metadata| metadata.email == email) {
            return Ok(Some(metadata));
        }
        let identity = self.identity;
        let selected = ReadRequest {
            epoch: identity.epoch,
            account: identity.account,
            email,
            maximum: postgres_body::MAX_BODY_BYTES,
        };
        let result = self
            .worker
            .run(|client| {
                Box::pin(async move {
                    match postgres_body::email_metadata(client, selected).await {
                        Ok(metadata) if metadata.identity == identity => Ok(Some(metadata)),
                        Ok(_) => Err(Error::Integrity),
                        Err(postgres_body::Error::Missing) => Ok(None),
                        Err(error) => Err(Error::Body(error)),
                    }
                })
            })
            .map_err(Error::Connection)
            .and_then(|result| result);
        let metadata = self.retain(result)?;
        self.metadata = metadata;
        Ok(metadata)
    }
    pub fn with_body<T>(
        &mut self,
        email: EmailId,
        read: impl FnOnce(Metadata, &mut dyn BlobReader) -> Result<T, ports::Error>,
    ) -> Result<T, Error> {
        self.with_pinned_body(email, |metadata, pin| read(metadata, pin))
    }
    /// Retains a fully verified body for existing bounded MIME readers.
    /// The pin cannot escape this callback or renew its snapshot deadline.
    pub fn with_pinned_body<T>(
        &mut self,
        email: EmailId,
        read: impl FnOnce(Metadata, &mut crate::store_fs::PinnedBlob<'_, '_>) -> Result<T, ports::Error>,
    ) -> Result<T, Error> {
        self.with_body_checks(email, false, read)
    }
    /// Interpret into provisional response storage, grouping resident checks.
    /// The callback must not publish bytes or cross a fresh admission boundary.
    /// Cached reads share the bounded check interval; misses and completion
    /// retain unconditional connection-health checks. Deadlines never defer.
    pub fn with_interpreted_body<T>(
        &mut self,
        email: EmailId,
        read: impl FnOnce(Metadata, &mut crate::store_fs::PinnedBlob<'_, '_>) -> Result<T, ports::Error>,
    ) -> Result<T, Error> {
        self.with_body_checks(email, true, read)
    }
    fn with_body_checks<T>(
        &mut self,
        email: EmailId,
        interpreted: bool,
        read: impl FnOnce(Metadata, &mut crate::store_fs::PinnedBlob<'_, '_>) -> Result<T, ports::Error>,
    ) -> Result<T, Error> {
        let metadata = self
            .email(email)?
            .ok_or(Error::Body(postgres_body::Error::Missing));
        let metadata = self.retain(metadata)?;
        let blob = BlobMetadata {
            identity: metadata.identity,
            blob: metadata.blob,
            body: metadata.body,
        };
        let result = if interpreted {
            postgres_body::with_interpreted_pin(&mut self.worker, blob, self.deadline, |body| {
                read(metadata, body)
            })
        } else {
            postgres_body::with_verified_pin(&mut self.worker, blob, self.deadline, |body| {
                read(metadata, body)
            })
        }
        .map_err(Error::Body);
        self.retain(result)
    }
    /// Empty values do not prove that the Email exists; call email separately.
    pub fn mailboxes(
        &mut self,
        email: EmailId,
        after: Option<MailboxId>,
        maximum: usize,
    ) -> Result<Values<MailboxId>, Error> {
        self.check()?;
        let result = self.value_limit(maximum);
        let limit = self.retain(result)?;
        let result = self
            .value_window
            .mailboxes(self.identity, email, after, maximum);
        if let Some(values) = self.retain(result)? {
            return Ok(values);
        }
        let identity = self.identity;
        let after = after.map(|value| *value.as_bytes());
        let result=self.worker.run(|client|Box::pin(async move {
            let rows=client.query_typed("SELECT m.mailbox_id,m.changed,b.changed FROM td_mta.memberships m LEFT JOIN td_mta.mailboxes b ON b.account=m.account AND b.id=m.mailbox_id WHERE m.account=$1 AND m.email_id=$2 AND ($3::bytea IS NULL OR m.mailbox_id>$3) ORDER BY m.mailbox_id LIMIT $4", &[(&identity.account.as_bytes().as_slice(),Type::BYTEA),(&email.as_bytes().as_slice(),Type::BYTEA),(&after.as_ref().map(|bytes|bytes.as_slice()),Type::BYTEA),(&limit,Type::INT8)]).await.map_err(|_|Error::Database)?;
            let mut values=Vec::new();
            values.try_reserve_exact(maximum).map_err(|_|Error::Capacity)?;
            let has_more=rows.len()>maximum;
            let mut previous=after;
            for row in rows {
                for column in [1,2] { check_changed(row.try_get(column).map_err(|_|Error::Integrity)?,identity)?; }
                let id:&[u8]=row.try_get(0).map_err(|_|Error::Integrity)?;
                let id:[u8;16]=id.try_into().map_err(|_|Error::Integrity)?;
                if previous.is_some_and(|previous|previous>=id) { return Err(Error::Integrity); }
                previous=Some(id);
                if values.len()<maximum { values.push(MailboxId::from_bytes(id)); }
            }
            Ok(Values { identity,email,values,has_more })
        })).map_err(Error::Connection).and_then(|result|result);
        self.retain(result)
    }
    pub fn keywords(
        &mut self,
        email: EmailId,
        after: Option<&str>,
        maximum: usize,
    ) -> Result<Values<String>, Error> {
        self.check()?;
        let result = self.value_limit(maximum);
        let limit = self.retain(result)?;
        if let Some(after) = after {
            let valid = Key::Keyword(email, after)
                .validate_local()
                .map_err(|_| Error::Invalid);
            self.retain(valid)?;
        }
        let result = self
            .value_window
            .keywords(self.identity, email, after, maximum);
        if let Some(values) = self.retain(result)? {
            return Ok(values);
        }
        let after = after.map(copy_text).transpose();
        let after = self.retain(after)?;
        let identity = self.identity;
        let result=self.worker.run(|client|Box::pin(async move {
            let rows=client.query_typed("SELECT keyword,changed FROM td_mta.keywords WHERE account=$1 AND email_id=$2 AND ($3::text IS NULL OR keyword>$3 COLLATE \"C\") ORDER BY keyword LIMIT $4", &[(&identity.account.as_bytes().as_slice(),Type::BYTEA),(&email.as_bytes().as_slice(),Type::BYTEA),(&after,Type::TEXT),(&limit,Type::INT8)]).await.map_err(|_|Error::Database)?;
            let mut values=Vec::new();
            values.try_reserve_exact(maximum).map_err(|_|Error::Capacity)?;
            let has_more=rows.len()>maximum;
            let mut previous=after.as_deref();
            for row in &rows {
                check_changed(row.try_get(1).map_err(|_|Error::Integrity)?,identity)?;
                let text:&str=row.try_get(0).map_err(|_|Error::Integrity)?;
                Key::Keyword(email,text).validate_local().map_err(|_|Error::Integrity)?;
                if previous.is_some_and(|previous|previous>=text) { return Err(Error::Integrity); }
                if values.len()<maximum { values.push(copy_text(text)?); }
                previous=Some(text);
            }
            Ok(Values { identity,email,values,has_more })
        })).map_err(Error::Connection).and_then(|result|result);
        self.retain(result)
    }
    /// Order received dates in the requested direction and equal dates by ID
    /// ascending, matching the existing td-mail query interpretation.
    pub fn query_emails(&mut self, query: EmailQuery) -> Result<EmailPage, Error> {
        self.check()?;
        if query.limit > MAX_EMAIL_PAGE {
            return self.retain(Err(Error::Capacity));
        }
        let identity = self.identity;
        let result = self.worker.run(|client| Box::pin(async move {
            let mailbox = match query.scope {
                EmailScope::Mailbox(id) => Some(*id.as_bytes()),
                _ => None,
            };
            let mut page = EmailPage {
                state: crate::sync::QueryState::current(identity, crate::sync::QueryType::Email),
                position: query.position,
                ids: Vec::new(),
                total: query.calculate_total.then_some(0),
                has_more: false,
            };
            if query.scope == EmailScope::Empty || matches!((query.after, query.before), (Some(after), Some(before)) if after >= before) { return Ok(page); }
            let account = identity.account.as_bytes().as_slice();
            let mailbox = mailbox.as_ref().map(|id|id.as_slice());
            if query.calculate_total {
                let row=client.query_typed_one("SELECT count(*) FROM td_mta.emails e WHERE e.account=$1 AND ($2::bytea IS NULL OR EXISTS (SELECT 1 FROM td_mta.memberships m WHERE m.account=e.account AND m.email_id=e.id AND m.mailbox_id=$2)) AND ($3::bigint IS NULL OR e.received_at >= $3) AND ($4::bigint IS NULL OR e.received_at < $4)", &[(&account,Type::BYTEA),(&mailbox,Type::BYTEA),(&query.after,Type::INT8),(&query.before,Type::INT8)]).await.map_err(|_|Error::Database)?;
                let total:i64=row.try_get(0).map_err(|_|Error::Integrity)?;
                page.total=Some(u64::try_from(total).map_err(|_|Error::Integrity)?);
            }
            let Ok(position)=i64::try_from(query.position) else { return Ok(page); };
            let limit=i64::try_from(query.limit+1).map_err(|_|Error::Capacity)?;
            let sql=if query.ascending {
                "SELECT e.id,e.received_at,e.changed,m.changed FROM td_mta.emails e LEFT JOIN td_mta.memberships m ON m.account=e.account AND m.email_id=e.id AND m.mailbox_id=$2 WHERE e.account=$1 AND ($2::bytea IS NULL OR m.email_id IS NOT NULL) AND ($5::bigint IS NULL OR e.received_at >= $5) AND ($6::bigint IS NULL OR e.received_at < $6) ORDER BY e.received_at ASC,e.id ASC LIMIT $3 OFFSET $4"
            } else {
                "SELECT e.id,e.received_at,e.changed,m.changed FROM td_mta.emails e LEFT JOIN td_mta.memberships m ON m.account=e.account AND m.email_id=e.id AND m.mailbox_id=$2 WHERE e.account=$1 AND ($2::bytea IS NULL OR m.email_id IS NOT NULL) AND ($5::bigint IS NULL OR e.received_at >= $5) AND ($6::bigint IS NULL OR e.received_at < $6) ORDER BY e.received_at DESC,e.id ASC LIMIT $3 OFFSET $4"
            };
            let rows=client.query_typed(sql,&[(&account,Type::BYTEA),(&mailbox,Type::BYTEA),(&limit,Type::INT8),(&position,Type::INT8),(&query.after,Type::INT8),(&query.before,Type::INT8)]).await.map_err(|_|Error::Database)?;
            page.ids.try_reserve_exact(query.limit).map_err(|_|Error::Capacity)?;
            page.has_more=rows.len()>query.limit;
            let mut previous:Option<(i64,[u8;16])>=None;
            for row in rows {
                let id:&[u8]=row.try_get(0).map_err(|_|Error::Integrity)?;
                let id:[u8;16]=id.try_into().map_err(|_|Error::Integrity)?;
                let date:i64=row.try_get(1).map_err(|_|Error::Integrity)?;
                check_changed(row.try_get(2).map_err(|_|Error::Integrity)?,identity)?;
                let membership:Option<&[u8]>=row.try_get(3).map_err(|_|Error::Integrity)?;
                match (mailbox,membership) {
                    (Some(_),Some(changed))=>check_changed(changed,identity)?,
                    (None,None)=>(),
                    _=>return Err(Error::Integrity),
                }
                if let Some((old_date,old_id))=previous {
                    let order=old_date.cmp(&date);
                    let order=if query.ascending {order} else {order.reverse()};
                    if !order.then_with(||old_id.cmp(&id)).is_lt() { return Err(Error::Integrity); }
                }
                previous=Some((date,id));
                if page.ids.len()<query.limit { page.ids.push(EmailId::from_bytes(id)); }
            }
            Ok(page)
        })).map_err(Error::Connection).and_then(|result|result);
        self.retain(result)
    }
    /// Ascending received-date/ID windows on this view's immutable thread
    /// assignment. An empty stored thread remains distinguishable from absence.
    pub fn thread_emails(
        &mut self,
        thread: ThreadId,
        after: Option<ports::ThreadEmail>,
        maximum: usize,
    ) -> Result<Option<ports::ThreadEmails>, Error> {
        self.check()?;
        let result = if maximum == 0 {
            Err(Error::Invalid)
        } else if maximum > ports::MAX_THREAD_EMAILS {
            Err(Error::Capacity)
        } else {
            i64::try_from(maximum + 1).map_err(|_| Error::Capacity)
        };
        let limit = self.retain(result)?;
        let identity = self.identity;
        let result=self.worker.run(|client|Box::pin(async move {
            let account=identity.account.as_bytes().as_slice();
            let thread_bytes=thread.as_bytes().as_slice();
            let row=client.query_typed_opt("SELECT changed FROM td_mta.threads WHERE account=$1 AND id=$2", &[(&account,Type::BYTEA),(&thread_bytes,Type::BYTEA)]).await.map_err(|_|Error::Database)?;
            let Some(row)=row else { return Ok(None); };
            check_changed(row.try_get(0).map_err(|_|Error::Integrity)?,identity)?;
            let date=after.map(|entry|entry.received_at);
            let id=after.map(|entry|*entry.email.as_bytes());
            let id=id.as_ref().map(|id|id.as_slice());
            let rows=client.query_typed("SELECT id,received_at,changed FROM td_mta.emails WHERE account=$1 AND thread_id=$2 AND ($3::bigint IS NULL OR (received_at,id)>($3,$4::bytea)) ORDER BY received_at ASC,id ASC LIMIT $5", &[(&account,Type::BYTEA),(&thread_bytes,Type::BYTEA),(&date,Type::INT8),(&id,Type::BYTEA),(&limit,Type::INT8)]).await.map_err(|_|Error::Database)?;
            let has_more=rows.len()>maximum;
            let mut emails=Vec::new();
            emails.try_reserve_exact(maximum).map_err(|_|Error::Capacity)?;
            let mut previous=after;
            for row in rows {
                let id:&[u8]=row.try_get(0).map_err(|_|Error::Integrity)?;
                let entry=ports::ThreadEmail {
                    email:EmailId::from_bytes(id.try_into().map_err(|_|Error::Integrity)?),
                    received_at:row.try_get(1).map_err(|_|Error::Integrity)?,
                };
                check_changed(row.try_get(2).map_err(|_|Error::Integrity)?,identity)?;
                if previous.is_some_and(|previous|previous>=entry) { return Err(Error::Integrity); }
                previous=Some(entry);
                if emails.len()<maximum { emails.push(entry); }
            }
            Ok(Some(ports::ThreadEmails {identity,thread,emails,has_more}))
        })).map_err(Error::Connection).and_then(|result|result);
        self.retain(result)
    }
    fn value_limit(&self, maximum: usize) -> Result<i64, Error> {
        if maximum == 0 {
            return Err(Error::Invalid);
        }
        if maximum > MAX_VALUES {
            return Err(Error::Capacity);
        }
        i64::try_from(maximum + 1).map_err(|_| Error::Capacity)
    }
}
fn copy_text(text: &str) -> Result<String, Error> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(text.len())
        .map_err(|_| Error::Capacity)?;
    owned.push_str(text);
    Ok(owned)
}
fn check_changed(bytes: &[u8], identity: ViewIdentity) -> Result<(), Error> {
    let sequence = u64::from_be_bytes(bytes.try_into().map_err(|_| Error::Integrity)?);
    if sequence > identity.committed_sequence.number() {
        return Err(Error::Integrity);
    }
    Ok(())
}
pub(crate) fn snapshot_error(error: postgres_snapshot::Error) -> Error {
    match error {
        postgres_snapshot::Error::Schema(value) => Error::Schema(value),
        postgres_snapshot::Error::Credential(value) => Error::Credential(value),
        postgres_snapshot::Error::Epoch => Error::Epoch,
        postgres_snapshot::Error::Policy => Error::Policy,
        postgres_snapshot::Error::Unauthenticated => Error::Unauthenticated,
        postgres_snapshot::Error::Integrity => Error::Integrity,
        postgres_snapshot::Error::Database => Error::Database,
    }
}

#[cfg(test)]
mod slot_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn read_capacity_is_fixed_and_released_by_each_owner() {
        assert!(ReadSlots::new(0).is_err());
        assert!(ReadSlots::new(257).is_err());
        let slots = ReadSlots::new(2).unwrap();
        let first = slots.acquire().unwrap();
        let second = slots.acquire().unwrap();
        assert!(matches!(slots.acquire(), Err(Error::Busy)));
        drop(first);
        let replacement = slots.acquire().unwrap();
        assert!(matches!(slots.acquire(), Err(Error::Busy)));
        drop(second);
        drop(replacement);
        assert_eq!(slots.used.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
}
