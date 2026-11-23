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

#[path = "postgres_read_search.rs"]
mod search_reads;

#[path = "postgres_read_values.rs"]
mod value_reads;

#[path = "postgres_read_replication.rs"]
mod replication_reads;
pub use replication_reads::{
    ReplicaInventoryCursor, ReplicaInventoryPage, MAX_REPLICA_INVENTORY_PAGE,
};

#[path = "postgres_read_submissions.rs"]
mod submission_reads;
pub use submission_reads::{
    SubmissionMetadata, SubmissionPage, SubmissionQuery, MAX_SUBMISSION_PAGE,
};

pub use crate::email_response::{EmailValues as Values, MAX_EMAIL_VALUES as MAX_VALUES};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
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
    pub(crate) fn at_capacity(&self) -> bool {
        self.used.load(std::sync::atomic::Ordering::Acquire) >= self.maximum
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
    summary_window: Option<crate::postgres_probe::Rows>,
    search_window: Option<search_reads::Window>,
    value_window: value_reads::Window,
    authentication_window: AuthenticationWindow,
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
    fn email_authentication(
        &mut self,
        email: EmailId,
    ) -> Result<Option<crate::sender_authentication::Results>, Error> {
        self.sender_authentication(email)
    }
}

type AuthenticationWindow = Vec<(
    EmailId,
    Option<Option<crate::sender_authentication::Results>>,
)>;

// One row per signature, or one row for an Email without signatures or results.
const AUTHENTICATION: &str = "SELECT a.spf_result,a.spf_domain,a.dkim_complete,a.dmarc_result,
    a.dmarc_author,a.dmarc_policy_domain,a.dmarc_policy,a.dmarc_strict_spf,a.dmarc_strict_dkim,
    a.dmarc_aligned,s.ordinal,s.result,s.algorithm,s.domain,s.selector,s.key_bits,a.spf_helo,i.id
    FROM unnest($2::bytea[]) i(id)
    LEFT JOIN td_mta.email_authentication a ON a.account=$1 AND a.email_id=i.id
    LEFT JOIN td_mta.email_dkim_signatures s ON s.account=a.account AND s.email_id=a.email_id
    ORDER BY i.id,s.ordinal LIMIT $3";

/// Groups AUTHENTICATION rows by requested Email; every requested ID must
/// appear once, in byte order, and nothing else.
fn authentication_window(
    rows: &crate::postgres_probe::Rows,
    ids: &[EmailId],
) -> Result<AuthenticationWindow, Error> {
    let mut window = td_alloc::with_capacity(ids.len()).map_err(|_| Error::Capacity)?;
    let id = |row: &crate::postgres_probe::Row| {
        row.try_get::<_, &[u8]>(17)
            .ok()
            .and_then(|bytes| <[u8; 16]>::try_from(bytes).ok())
            .map(EmailId::from_bytes)
            .ok_or(Error::Integrity)
    };
    let mut start = 0;
    while let Some(first) = rows.get(start) {
        let email = id(&first)?;
        let mut end = start + 1;
        while let Some(row) = rows.get(end) {
            if id(&row)? != email {
                break;
            }
            end += 1;
        }
        let group = rows.range(start, end).ok_or(Error::Integrity)?;
        if ids.get(window.len()) != Some(&email) {
            return Err(Error::Integrity);
        }
        let recorded = first
            .try_get::<_, Option<&str>>(0)
            .map_err(|_| Error::Integrity)?
            .is_some();
        let value = if recorded {
            Some(authentication(group)?.ok_or(Error::Integrity)?)
        } else if group.len() == 1 {
            None
        } else {
            return Err(Error::Integrity);
        };
        td_alloc::push(&mut window, (email, Some(value))).map_err(|_| Error::Capacity)?;
        start = end;
    }
    if window.len() != ids.len() {
        return Err(Error::Integrity);
    }
    Ok(window)
}

fn stored_text(row: &crate::postgres_probe::Row, index: usize) -> Result<Option<String>, Error> {
    row.try_get::<_, Option<&str>>(index)
        .map_err(|_| Error::Integrity)?
        .map(td_alloc::string)
        .transpose()
        .map_err(|_| Error::Capacity)
}

fn stored_word<T>(
    row: &crate::postgres_probe::Row,
    index: usize,
    parse: fn(&str) -> Option<T>,
) -> Result<Option<T>, Error> {
    row.try_get::<_, Option<&str>>(index)
        .map_err(|_| Error::Integrity)?
        .map(|text| parse(text).ok_or(Error::Integrity))
        .transpose()
}

/// Rows from AUTHENTICATION; any value the schema should have refused is
/// an integrity failure rather than a partial result.
fn authentication(
    rows: crate::postgres_probe::RowIter<'_>,
) -> Result<Option<crate::sender_authentication::Results>, Error> {
    use crate::{dkim, dmarc, sender_authentication::stored};
    let Some(first) = rows.peek() else {
        return Ok(None);
    };
    if rows.len() > dkim::MAX_SIGNATURES {
        return Err(Error::Integrity);
    }
    let mut signatures = td_alloc::with_capacity(rows.len()).map_err(|_| Error::Capacity)?;
    for row in rows {
        let Some(ordinal) = row
            .try_get::<_, Option<i64>>(10)
            .map_err(|_| Error::Integrity)?
        else {
            continue;
        };
        if usize::try_from(ordinal).ok() != Some(signatures.len()) {
            return Err(Error::Integrity);
        }
        let bits = row
            .try_get::<_, Option<i32>>(15)
            .map_err(|_| Error::Integrity)?
            .map(|bits| u16::try_from(bits).map_err(|_| Error::Integrity))
            .transpose()?;
        td_alloc::push(
            &mut signatures,
            dkim::SignatureResult {
                outcome: stored_word(&row, 11, stored::dkim)?.ok_or(Error::Integrity)?,
                algorithm: stored_word(&row, 12, stored::algorithm)?,
                domain: stored_text(&row, 13)?,
                selector: stored_text(&row, 14)?,
                key_bits: bits,
            },
        )
        .map_err(|_| Error::Capacity)?;
    }
    let flag = |index| {
        first
            .try_get::<_, Option<bool>>(index)
            .map_err(|_| Error::Integrity)
    };
    let disposition = stored_word(&first, 6, stored::disposition)?;
    let policy = match (disposition, flag(7)?, flag(8)?) {
        (Some(disposition), Some(strict_spf), Some(strict_dkim)) => Some(dmarc::Requested {
            disposition,
            strict_spf,
            strict_dkim,
        }),
        (None, None, None) => None,
        _ => return Err(Error::Integrity),
    };
    let results = crate::sender_authentication::Results {
        spf: stored_word(&first, 0, stored::spf)?.ok_or(Error::Integrity)?,
        spf_domain: stored_text(&first, 1)?,
        spf_helo: first.try_get::<_, bool>(16).map_err(|_| Error::Integrity)?,
        dkim: dkim::Report {
            signatures,
            incomplete: !first.try_get::<_, bool>(2).map_err(|_| Error::Integrity)?,
        },
        dmarc: dmarc::Report {
            outcome: stored_word(&first, 3, stored::dmarc)?.ok_or(Error::Integrity)?,
            author: stored_text(&first, 4)?,
            policy_domain: stored_text(&first, 5)?,
            policy,
            aligned: stored_word(&first, 9, stored::identifier)?,
        },
    };
    if !results.readable() {
        return Err(Error::Integrity);
    }
    Ok(Some(results))
}

pub fn with_snapshot<T>(
    request: &Request<'_>,
    deadline: Instant,
    binding: &Binding,
    password: &[u8],
    read: impl FnOnce(&mut Snapshot<'_>) -> Result<T, Error>,
) -> Result<T, Error> {
    // Existing preparation callers already reserve their worker externally.
    let slots = ReadSlots::new(1)?;
    let mut view = Snapshot::open(&slots, request, deadline, binding, password)?;
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
        deadline: Instant,
        binding: &Binding,
        password: &[u8],
    ) -> Result<Self, Error> {
        if request.mode != binding.runtime_mode() {
            return Err(Error::Invalid);
        }
        Self::open(slots, request, deadline, binding, password)
    }
    /// Primary snapshot admission over the bootstrap's socket, in one reserved
    /// slot and the original allowance; policy, authentication and database
    /// failures refuse.
    pub fn open_loaded(
        slots: &'a ReadSlots,
        loaded: &crate::postgres_input::Loaded,
        deadline: Instant,
        binding: &Binding,
        password: &[u8],
    ) -> Result<Self, Error> {
        if loaded.bootstrap.profile() != binding.profile() {
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
        .reopen(loaded, password)
    }
    fn open(
        slots: &'a ReadSlots,
        request: &Request<'_>,
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
        let (worker, identity) = connect_authenticated(request, deadline, binding, presented)?;
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
            summary_window: None,
            search_window: None,
            value_window: value_reads::Window::default(),
            authentication_window: Vec::new(),
            _worker_thread: std::marker::PhantomData,
            _permit: permit,
        };
        view.check()?;
        Ok(view)
    }
}
fn connect_authenticated(
    request: &Request<'_>,
    deadline: Instant,
    binding: &Binding,
    presented: Presented,
) -> Result<(Worker, ViewIdentity), Error> {
    let binding = binding.clone();
    let mut worker = postgres_probe::body_worker(request, deadline).map_err(Error::Connection)?;
    let identity = worker
        .run(|client| postgres_snapshot::begin(client, &binding, presented).map_err(snapshot_error))
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
    /// Runtime-role qualification and full credential authentication on the
    /// original profile/account binding and reserved capacity.
    pub fn reopen(
        self,
        loaded: &crate::postgres_input::Loaded,
        password: &[u8],
    ) -> Result<Snapshot<'a>, Error> {
        if loaded.bootstrap.profile() != self.binding.profile() {
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
        let mut request = loaded.bootstrap.request();
        request.mode = self.binding.runtime_mode();
        let result = connect_authenticated(&request, self.deadline, &self.binding, presented);
        if Instant::now() >= self.deadline {
            return Err(Error::Connection(postgres_probe::Error::Deadline));
        }
        let (worker, identity) = result?;
        Snapshot::from_worker(
            self.permit,
            worker,
            identity,
            self.deadline,
            &self.binding,
            self.device,
        )
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
            .run(|client| postgres_mailboxes::read(client, identity, deadline))
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
                    postgres_body::email_metadata_batch(
                        client,
                        identity.epoch,
                        identity.account,
                        postgres_body::MAX_BODY_BYTES,
                        &ids,
                    )
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
        self.summary_window = None;
        self.search_window = None;
        self.value_window = value_reads::Window::default();
        self.authentication_window = Vec::new();
        self.metadata = None;
        Ok(())
    }
    /// Passive derived values never authorize a body read or replace current
    /// Email membership, device authorization, or mutable metadata.
    pub(crate) fn summary(
        &mut self,
        metadata: Metadata,
        header_limit: usize,
    ) -> Result<Option<Vec<Vec<u8>>>, Error> {
        self.check()?;
        if header_limit != crate::postgres_summaries::HEADER_LIMIT {
            return Ok(None);
        }
        if self.summary_window.is_none() {
            let mut blobs = Vec::new();
            blobs
                .try_reserve_exact(postgres_body::MAX_METADATA_IDS)
                .map_err(|_| Error::Capacity)?;
            for (_, value) in self.metadata_window.iter().flatten() {
                if let Some(value) = value {
                    if !blobs.contains(&value.blob) {
                        blobs.push(value.blob);
                    }
                }
            }
            if blobs.is_empty() {
                blobs.push(metadata.blob);
            }
            let account = self.identity.account;
            let result = self.worker.run(|client| {
                let mut ids = Vec::new();
                ids.try_reserve_exact(blobs.len()).map_err(|_| Error::Capacity)?;
                ids.extend(blobs.iter().map(|blob| blob.as_bytes().as_slice()));
                client.query("SELECT blob_id,body_length,body_digest,fields FROM td_mta.blob_summaries WHERE account=$1 AND blob_id=ANY($2) AND version=$3 ORDER BY blob_id LIMIT 16", &[&account.as_bytes().as_slice(), &ids, &crate::postgres_summaries::VERSION]).map_err(|_| Error::Database)
            }).map_err(Error::Connection).and_then(|rows| rows);
            self.summary_window = Some(self.retain(result)?);
        }
        let result = (|| {
            for row in self
                .summary_window
                .iter()
                .flat_map(crate::postgres_probe::Rows::iter)
            {
                let id: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
                if id != metadata.blob.as_bytes() {
                    continue;
                }
                let length: i64 = row.try_get(1).map_err(|_| Error::Integrity)?;
                let digest: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
                if u64::try_from(length).ok() != Some(metadata.body.length)
                    || digest != metadata.body.digest
                {
                    return Ok(None);
                }
                let fields: Vec<Vec<u8>> = row.try_get(3).map_err(|_| Error::Integrity)?;
                if !crate::postgres_summaries::valid(&fields) {
                    return Ok(None);
                }
                return Ok(Some(fields));
            }
            Ok(None)
        })();
        self.retain(result)
    }
    pub(crate) fn search_fields(
        &mut self,
        metadata: Metadata,
        limits: &crate::limits::Limits,
        work: &mut crate::admission::work::Meter,
        clock: &dyn ports::Clock,
    ) -> Result<Option<crate::jmap_request::dispatch::SearchFields>, Error> {
        use crate::postgres_search;
        self.check()?;
        if !postgres_search::compatible(limits) {
            return Ok(None);
        }
        let account = self.identity.account;
        let blob = metadata.blob;
        let Some(descriptor) = self.search_descriptor(blob)? else {
            return Ok(None);
        };
        if descriptor.length != metadata.body.length
            || descriptor.body_digest != metadata.body.digest
        {
            return Ok(None);
        }
        let size = descriptor.size;
        let expected = descriptor.expected;
        let count = size.div_ceil(65536) as i32;
        // Charge all retained bytes before transfer/hash/decode, even when
        // matching later succeeds in the first field or batch.
        work.charge(
            clock.monotonic().map_err(Error::Output)?,
            crate::admission::work::Charge {
                io_bytes: 4 * size as u64,
                records: 1 + size.div_ceil(4096) as u64,
                ..Default::default()
            },
        )
        .map_err(|stop| match stop {
            crate::admission::work::Stop::Deadline => {
                Error::Connection(postgres_probe::Error::Deadline)
            }
            _ => Error::Capacity,
        })?;
        let result = self.worker.run(|client| {
            client.query("SELECT n,substring(s.fields FROM n*65536+1 FOR 65536) FROM td_mta.blob_search s CROSS JOIN generate_series(0,$3-1) n WHERE s.account=$1 AND s.blob_id=$2 ORDER BY n LIMIT 132", &[&account.as_bytes().as_slice(), &blob.as_bytes().as_slice(), &count]).map_err(|_| Error::Database)
        }).map_err(Error::Connection).and_then(|rows| rows);
        let rows = self.retain(result)?;
        if rows.len() != count as usize {
            return Ok(None);
        }
        let mut encoded = Vec::new();
        encoded
            .try_reserve_exact(size)
            .map_err(|_| Error::Capacity)?;
        for (index, row) in rows.iter().enumerate() {
            self.check()?;
            let ordinal: i32 = row.try_get(0).map_err(|_| Error::Integrity)?;
            let bytes: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
            if ordinal as usize != index || bytes.len() != (size - encoded.len()).min(65536) {
                return Ok(None);
            }
            encoded.extend_from_slice(bytes);
        }
        drop(rows);
        if encoded.len() != size
            || postgres_search::digest(
                &metadata.body.digest,
                metadata.body.length,
                &encoded,
                self.deadline,
            )
            .ok()
            .as_ref()
            .map(|hash| hash.as_slice())
                != Some(expected.as_slice())
        {
            self.check()?;
            return Ok(None);
        }
        use crate::jmap_request::dispatch::Error as JmapError;
        let fields = crate::jmap_request::dispatch::SearchFields::decode_charged(&encoded, || {
            work.charge(
                clock.monotonic().map_err(JmapError::Storage)?,
                crate::admission::work::Charge {
                    records: 1,
                    ..Default::default()
                },
            )
            .map_err(|stop| match stop {
                crate::admission::work::Stop::Deadline => JmapError::ExecutionDeadline,
                _ => JmapError::StorageCapacity,
            })
        });
        let fields = match fields {
            Ok(fields) => Some(fields),
            Err(JmapError::ExecutionDeadline) => {
                return Err(Error::Connection(postgres_probe::Error::Deadline))
            }
            Err(JmapError::StorageCapacity) => return Err(Error::Capacity),
            Err(JmapError::Storage(error)) => return Err(Error::Output(error)),
            Err(_) => None,
        };
        self.check()?;
        Ok(fields)
    }

    pub(crate) fn mime_evidence(
        &mut self,
        metadata: Metadata,
        limits: &crate::limits::Limits,
    ) -> Result<Option<crate::postgres_mime_evidence::Evidence>, Error> {
        use crate::postgres_mime_evidence as mime;
        self.check()?;
        if limits.header_bytes != mime::HEADER_BYTES
            || limits.mime_parts != mime::PARTS
            || limits.mime_depth != mime::DEPTH
        {
            return Ok(None);
        }
        let account = self.identity.account;
        let blob = metadata.blob;
        let result = self.worker.run(|client| {
            client.query_opt("SELECT body_length,body_digest,header_bytes,part_table,chunk_hashes,manifest_digest FROM td_mta.blob_interpretations WHERE account=$1 AND blob_id=$2 AND version=$3", &[&account.as_bytes().as_slice(), &blob.as_bytes().as_slice(), &mime::VERSION]).map_err(|_| Error::Database)
        }).map_err(Error::Connection).and_then(|row| row);
        let Some(row) = self.retain(result)? else {
            return Ok(None);
        };
        let evidence = (|| {
            let length: i64 = row.try_get(0).ok()?;
            let digest: &[u8] = row.try_get(1).ok()?;
            if u64::try_from(length).ok()? != metadata.body.length || digest != metadata.body.digest
            {
                return None;
            }
            let headers: i64 = row.try_get(2).ok()?;
            let prepared = mime::Prepared {
                header_bytes: u64::try_from(headers).ok()?,
                table: row.try_get(3).ok()?,
                chunks: row.try_get(4).ok()?,
                digest: row.try_get::<_, &[u8]>(5).ok()?.try_into().ok()?,
            };
            mime::validate(metadata.body.length, &metadata.body.digest, prepared)
        })();
        self.check()?;
        Ok(evidence)
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
            .run(|client| value_reads::Window::read(client, identity, &ids, mailboxes, keywords))
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
            .run(
                |client| match postgres_body::email_metadata(client, selected) {
                    Ok(metadata) if metadata.identity == identity => Ok(Some(metadata)),
                    Ok(_) => Err(Error::Integrity),
                    Err(postgres_body::Error::Missing) => Ok(None),
                    Err(error) => Err(Error::Body(error)),
                },
            )
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
        self.with_body_checks(email, false, None, read)
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
        self.with_body_checks(email, true, None, read)
    }
    /// Qualified projection evidence permits per-chunk verification. This still
    /// builds only provisional output under the current authenticated snapshot.
    pub(crate) fn with_projected_body<T>(
        &mut self,
        email: EmailId,
        chunks: Option<&crate::postgres_mime_evidence::ChunkEvidence>,
        read: impl FnOnce(Metadata, &mut crate::store_fs::PinnedBlob<'_, '_>) -> Result<T, ports::Error>,
    ) -> Result<T, Error> {
        self.with_body_checks(email, true, chunks, read)
    }
    fn with_body_checks<T>(
        &mut self,
        email: EmailId,
        interpreted: bool,
        chunks: Option<&crate::postgres_mime_evidence::ChunkEvidence>,
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
        let result = if let Some(chunks) = chunks {
            postgres_body::with_chunk_verified_pin(
                &mut self.worker,
                blob,
                self.deadline,
                chunks,
                |body| read(metadata, body),
            )
        } else if interpreted {
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
        let result=self.worker.run(|client|{
            let rows=client.query("SELECT m.mailbox_id,m.changed,b.changed FROM td_mta.memberships m LEFT JOIN td_mta.mailboxes b ON b.account=m.account AND b.id=m.mailbox_id WHERE m.account=$1 AND m.email_id=$2 AND ($3::bytea IS NULL OR m.mailbox_id>$3) ORDER BY m.mailbox_id LIMIT $4", &[&identity.account.as_bytes().as_slice(), &email.as_bytes().as_slice(), &after.as_ref().map(|bytes|bytes.as_slice()), &limit]).map_err(|_|Error::Database)?;
            let mut values=Vec::new();
            values.try_reserve_exact(maximum).map_err(|_|Error::Capacity)?;
            let has_more=rows.len()>maximum;
            let mut previous=after;
            for row in &rows {
                for column in [1,2] { check_changed(row.try_get(column).map_err(|_|Error::Integrity)?,identity)?; }
                let id:&[u8]=row.try_get(0).map_err(|_|Error::Integrity)?;
                let id:[u8;16]=id.try_into().map_err(|_|Error::Integrity)?;
                if previous.is_some_and(|previous|previous>=id) { return Err(Error::Integrity); }
                previous=Some(id);
                if values.len()<maximum { values.push(MailboxId::from_bytes(id)); }
            }
            Ok(Values { identity,email,values,has_more })
        }).map_err(Error::Connection).and_then(|result|result);
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
        let result=self.worker.run(|client|{
            let rows=client.query("SELECT keyword,changed FROM td_mta.keywords WHERE account=$1 AND email_id=$2 AND ($3::text IS NULL OR keyword>$3 COLLATE \"C\") ORDER BY keyword LIMIT $4", &[&identity.account.as_bytes().as_slice(), &email.as_bytes().as_slice(), &after, &limit]).map_err(|_|Error::Database)?;
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
        }).map_err(Error::Connection).and_then(|result|result);
        self.retain(result)
    }
    /// The receiver's own direct-MX results for one Email of this account,
    /// from the prefetched window when it holds them.
    pub(crate) fn sender_authentication(
        &mut self,
        email: EmailId,
    ) -> Result<Option<crate::sender_authentication::Results>, Error> {
        self.check()?;
        let cached = self
            .authentication_window
            .iter_mut()
            .find(|(id, slot)| *id == email && slot.is_some())
            .and_then(|(_, slot)| slot.take());
        if let Some(value) = cached {
            return Ok(value);
        }
        self.prefetch_authentication(&[email])?;
        let value = self
            .authentication_window
            .iter_mut()
            .find(|(id, _)| *id == email)
            .and_then(|(_, slot)| slot.take())
            .ok_or(Error::Integrity);
        self.retain(value)
    }
    /// One account-scoped query for up to sixteen Emails' results; each
    /// cached value is read at most once.
    pub(crate) fn prefetch_authentication(&mut self, emails: &[EmailId]) -> Result<(), Error> {
        self.check()?;
        self.authentication_window = Vec::new();
        if emails.len() > postgres_body::MAX_METADATA_IDS {
            return self.retain(Err(Error::Capacity));
        }
        let mut ids = td_alloc::with_capacity(emails.len()).map_err(|_| Error::Capacity);
        if let Ok(ids) = ids.as_mut() {
            for email in emails {
                if !ids.contains(email) && td_alloc::push(ids, *email).is_err() {
                    return self.retain(Err(Error::Capacity));
                }
            }
            ids.sort_unstable_by(|a: &EmailId, b| a.as_bytes().cmp(b.as_bytes()));
        }
        let ids = self.retain(ids)?;
        if ids.is_empty() {
            return Ok(());
        }
        let identity = self.identity;
        let result = self
            .worker
            .run(|client| {
                let mut keys = td_alloc::with_capacity(ids.len()).map_err(|_| Error::Capacity)?;
                for id in &ids {
                    td_alloc::push(&mut keys, id.as_bytes().as_slice())
                        .map_err(|_| Error::Capacity)?;
                }
                let limit = i64::try_from(ids.len())
                    .ok()
                    .and_then(|n| n.checked_mul(4))
                    .and_then(|n| n.checked_add(1))
                    .ok_or(Error::Capacity)?;
                let rows = client
                    .query(
                        AUTHENTICATION,
                        &[&identity.account.as_bytes().as_slice(), &keys, &limit],
                    )
                    .map_err(|_| Error::Database)?;
                authentication_window(&rows, &ids)
            })
            .map_err(Error::Connection)
            .and_then(|result| result);
        self.authentication_window = self.retain(result)?;
        Ok(())
    }
    /// Order received dates in the requested direction and equal dates by ID
    /// ascending, matching the existing td-mail query interpretation.
    pub fn query_emails(&mut self, query: EmailQuery) -> Result<EmailPage, Error> {
        self.query_emails_filtered(query, None)
    }
    pub(crate) fn search_candidates(
        &mut self,
        query: EmailQuery,
        pattern: Option<&str>,
    ) -> Result<EmailPage, Error> {
        if query.calculate_total {
            return self.retain(Err(Error::Invalid));
        }
        let pattern = if let Some(pattern) = pattern {
            if pattern.len() > 20482 || pattern.contains('\0') {
                return self.retain(Err(Error::Invalid));
            }
            let mut owned = String::new();
            owned
                .try_reserve_exact(pattern.len())
                .map_err(|_| Error::Capacity)?;
            owned.push_str(pattern);
            Some(owned)
        } else {
            None
        };
        self.query_emails_filtered(query, pattern)
    }
    fn query_emails_filtered(
        &mut self,
        query: EmailQuery,
        pattern: Option<String>,
    ) -> Result<EmailPage, Error> {
        self.check()?;
        if query.limit > MAX_EMAIL_PAGE {
            return self.retain(Err(Error::Capacity));
        }
        let identity = self.identity;
        let result = self.worker.run(|client| {
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
                let row=client.query_one("SELECT count(*) FROM td_mta.emails e WHERE e.account=$1 AND ($2::bytea IS NULL OR EXISTS (SELECT 1 FROM td_mta.memberships m WHERE m.account=e.account AND m.email_id=e.id AND m.mailbox_id=$2)) AND ($3::bigint IS NULL OR e.received_at >= $3) AND ($4::bigint IS NULL OR e.received_at < $4)", &[&account, &mailbox, &query.after, &query.before]).map_err(|_|Error::Database)?;
                let total:i64=row.try_get(0).map_err(|_|Error::Integrity)?;
                page.total=Some(u64::try_from(total).map_err(|_|Error::Integrity)?);
            }
            let Ok(position)=i64::try_from(query.position) else { return Ok(page); };
            let limit=i64::try_from(query.limit+1).map_err(|_|Error::Capacity)?;
            let sql = match (pattern.is_some(), query.ascending) {
                (false, true) => include_str!("postgres_email_page_asc.sql"),
                (false, false) => include_str!("postgres_email_page_desc.sql"),
                (true, true) => include_str!("postgres_search_candidates_asc.sql"),
                (true, false) => include_str!("postgres_search_candidates_desc.sql"),
            };
            let parameters: [&dyn crate::postgres_probe::Encode; 8] = [
                &account, &mailbox, &limit, &position, &query.after, &query.before,
                &pattern, &crate::postgres_search::VERSION];
            let parameters = parameters.get(..if pattern.is_some() { 8 } else { 6 }).ok_or(Error::Invalid)?;
            let rows=client.query(sql,parameters).map_err(|_|Error::Database)?;
            page.ids.try_reserve_exact(query.limit).map_err(|_|Error::Capacity)?;
            page.has_more=rows.len()>query.limit;
            let mut previous:Option<(i64,[u8;16])>=None;
            for row in &rows {
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
        }).map_err(Error::Connection).and_then(|result|result);
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
        let result=self.worker.run(|client|{
            let account=identity.account.as_bytes().as_slice();
            let thread_bytes=thread.as_bytes().as_slice();
            let row=client.query_opt("SELECT changed FROM td_mta.threads WHERE account=$1 AND id=$2", &[&account, &thread_bytes]).map_err(|_|Error::Database)?;
            let Some(row)=row else { return Ok(None); };
            check_changed(row.try_get(0).map_err(|_|Error::Integrity)?,identity)?;
            let date=after.map(|entry|entry.received_at);
            let id=after.map(|entry|*entry.email.as_bytes());
            let id=id.as_ref().map(|id|id.as_slice());
            let rows=client.query("SELECT id,received_at,changed FROM td_mta.emails WHERE account=$1 AND thread_id=$2 AND ($3::bigint IS NULL OR (received_at,id)>($3,$4::bytea)) ORDER BY received_at ASC,id ASC LIMIT $5", &[&account, &thread_bytes, &date, &id, &limit]).map_err(|_|Error::Database)?;
            let has_more=rows.len()>maximum;
            let mut emails=Vec::new();
            emails.try_reserve_exact(maximum).map_err(|_|Error::Capacity)?;
            let mut previous=after;
            for row in &rows {
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
        }).map_err(Error::Connection).and_then(|result|result);
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
