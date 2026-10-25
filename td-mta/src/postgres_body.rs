//! Verified Email body reads under one authenticated PostgreSQL snapshot.
use crate::{
    device_credentials::Presented,
    format::{row::BlobRow, Sequence},
    ids::{AccountId, BlobId, EmailId, StoreEpoch, ThreadId},
    ports::{self, BlobReader, ViewIdentity},
    postgres_devices,
    postgres_probe::{self, Request, Worker},
    postgres_runtime_profile::Binding,
    postgres_schema,
};
use std::time::Instant;
use td_crypto::{Crypto, Digest, Provider, TrustStore};
use tokio_postgres::types::Type;

const CHUNK: u64 = 65536;
const BATCH: u64 = 8;
pub const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_METADATA_IDS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Credential(postgres_devices::Error),
    Epoch,
    Policy,
    Unauthenticated,
    Missing,
    Integrity,
    Capacity,
    Database,
    Read(ports::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadRequest {
    pub epoch: StoreEpoch,
    pub account: AccountId,
    pub email: EmailId,
    pub maximum: u64,
}

/// Passive values from the same snapshot as the verified body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metadata {
    pub identity: ViewIdentity,
    pub email: EmailId,
    pub blob: BlobId,
    pub thread: ThreadId,
    pub received_at: i64,
    pub body: BlobRow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobMetadata {
    pub identity: ViewIdentity,
    pub blob: BlobId,
    pub body: BlobRow,
}

pub struct BlobRequest<'a> {
    pub binding: &'a Binding,
    pub blob: BlobId,
    pub maximum: u64,
}

/// Read only a body referenced by the selected account's Email. Upload leases
/// and standalone transmitted blobs are outside this operation's authority.
/// The callback sees verified bytes through the same snapshot, and must bound
/// all its own work and I/O by the original deadline. No snapshot escapes.
pub fn with_email<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    selected: ReadRequest,
    password: &[u8],
    read: impl FnOnce(Metadata, &mut dyn BlobReader) -> Result<T, ports::Error>,
) -> Result<T, Error> {
    if selected.maximum > MAX_BODY_BYTES {
        return Err(Error::Capacity);
    }
    let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
    postgres_probe::with_body_worker(request, trust, deadline, |worker| {
        let metadata = worker.run(|client| Box::pin(async move {
            let schema = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if schema.store_epoch != selected.epoch { return Err(Error::Epoch); }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)?;
            postgres_devices::authenticate(client, selected.account, presented).await.map_err(Error::Credential)?.ok_or(Error::Unauthenticated)?;
            email_metadata(client,selected).await
        })).map_err(Error::Connection)??;
        with_verified_body(worker,BlobMetadata { identity:metadata.identity,blob:metadata.blob,body:metadata.body },deadline,|body|read(metadata,body))
    }).map_err(Error::Connection)?
}

pub(crate) async fn email_metadata(
    client: &tokio_postgres::Client,
    selected: ReadRequest,
) -> Result<Metadata, Error> {
    email_metadata_batch(
        client,
        selected.epoch,
        selected.account,
        selected.maximum,
        &[selected.email],
    )
    .await?
    .pop()
    .ok_or(Error::Missing)
}

pub(crate) async fn email_metadata_batch(
    client: &tokio_postgres::Client,
    epoch: StoreEpoch,
    account: AccountId,
    maximum: u64,
    emails: &[EmailId],
) -> Result<Vec<Metadata>, Error> {
    if emails.len() > MAX_METADATA_IDS || maximum > MAX_BODY_BYTES {
        return Err(Error::Capacity);
    }
    if emails.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    ids.try_reserve_exact(emails.len())
        .map_err(|_| Error::Capacity)?;
    ids.extend(emails.iter().map(|id| id.as_bytes().as_slice()));
    let rows = client.query_typed("SELECT a.sequence,a.floor,e.blob_id,e.thread_id,e.received_at,b.length,b.digest,b.created_at,b.chunk_count,e.changed,b.changed,t.changed,(SELECT count(*) FROM td_mta.blob_chunks c WHERE c.account=a.id AND c.blob_id=e.blob_id),e.id FROM td_mta.accounts a JOIN td_mta.emails e ON e.account=a.id LEFT JOIN td_mta.blobs b ON b.account=e.account AND b.id=e.blob_id LEFT JOIN td_mta.threads t ON t.account=e.account AND t.id=e.thread_id WHERE a.id=$1 AND e.id=ANY($2) ORDER BY e.id LIMIT 17", &[(&account.as_bytes().as_slice(), Type::BYTEA), (&ids, Type::BYTEA_ARRAY)]).await.map_err(|_| Error::Database)?;
    if rows.len() > emails.len() {
        return Err(Error::Integrity);
    }
    let mut values = Vec::new();
    values
        .try_reserve_exact(rows.len())
        .map_err(|_| Error::Capacity)?;
    let mut previous: Option<EmailId> = None;
    for row in rows {
        let bytes: &[u8] = row.try_get(13).map_err(|_| Error::Integrity)?;
        let email = EmailId::from_bytes(bytes.try_into().map_err(|_| Error::Integrity)?);
        if !emails.contains(&email) || previous.is_some_and(|id| id.as_bytes() >= email.as_bytes())
        {
            return Err(Error::Integrity);
        }
        values.push(metadata(
            &row,
            ReadRequest {
                epoch,
                account,
                email,
                maximum,
            },
        )?);
        previous = Some(email);
    }
    Ok(values)
}

pub(crate) fn with_verified_body<T>(
    worker: &mut Worker,
    metadata: BlobMetadata,
    deadline: Instant,
    read: impl FnOnce(&mut dyn BlobReader) -> Result<T, ports::Error>,
) -> Result<T, Error> {
    with_verified_pin(worker, metadata, deadline, |pin| read(pin))
}

pub(crate) fn with_verified_pin<T>(
    worker: &mut Worker,
    metadata: BlobMetadata,
    deadline: Instant,
    read: impl FnOnce(&mut crate::store_fs::PinnedBlob<'_, '_>) -> Result<T, ports::Error>,
) -> Result<T, Error> {
    with_pin_checks(worker, metadata, deadline, false, read)
}

pub(crate) fn with_interpreted_pin<T>(
    worker: &mut Worker,
    metadata: BlobMetadata,
    deadline: Instant,
    read: impl FnOnce(&mut crate::store_fs::PinnedBlob<'_, '_>) -> Result<T, ports::Error>,
) -> Result<T, Error> {
    with_pin_checks(worker, metadata, deadline, true, read)
}

fn with_pin_checks<T>(
    worker: &mut Worker,
    metadata: BlobMetadata,
    deadline: Instant,
    interpreted: bool,
    read: impl FnOnce(&mut crate::store_fs::PinnedBlob<'_, '_>) -> Result<T, ports::Error>,
) -> Result<T, Error> {
    let mut body = Body {
        worker,
        metadata,
        deadline,
        failed: None,
        cache: Vec::new(),
        cache_start: 0,
        checks_left: interpreted.then_some(0),
    };
    body.verify().map_err(Error::Read)?;
    let mut pin =
        crate::store_fs::PinnedBlob::from_verified(&mut body, metadata.blob, metadata.body)
            .map_err(Error::Read)?;
    let result = read(&mut pin);
    pin.check_deadline().map_err(Error::Read)?;
    drop(pin);
    body.check().map_err(Error::Read)?;
    result.map_err(Error::Read)
}

/// Verify and replay one immutable snapshot. Account-owned Email/submission
/// references admit any current account device; an otherwise unreferenced
/// upload requires that device's unexpired lease. Reauthorize before callback
/// admission; an admitted callback retains its original finite deadline.
pub fn with_blob<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    selected: BlobRequest<'_>,
    password: &[u8],
    read: impl FnOnce(BlobMetadata, &mut dyn BlobReader) -> Result<T, ports::Error>,
) -> Result<T, Error> {
    with_pinned_blob(
        request,
        trust,
        deadline,
        selected,
        password,
        |metadata, pin| read(metadata, pin),
    )
}

/// Download custody for the existing MIME-part readers. Fresh primary
/// reauthorization still follows verification and precedes the callback.
pub fn with_pinned_blob<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    selected: BlobRequest<'_>,
    password: &[u8],
    read: impl FnOnce(BlobMetadata, &mut crate::store_fs::PinnedBlob<'_, '_>) -> Result<T, ports::Error>,
) -> Result<T, Error> {
    let binding = selected.binding;
    if !binding.roles().jmap() {
        return Err(Error::Policy);
    }
    if selected.maximum > MAX_BODY_BYTES {
        return Err(Error::Capacity);
    }
    let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
    let snapshot_binding = binding.clone();
    postgres_probe::with_body_worker(request, trust, deadline, |worker| {
        let (metadata, expires) = worker
            .run(|client| {
                Box::pin(blob_snapshot(
                    client,
                    snapshot_binding,
                    selected.blob,
                    selected.maximum,
                    presented,
                ))
            })
            .map_err(Error::Connection)??;
        let mut body = Body {
            worker,
            metadata,
            deadline,
            failed: None,
            cache: Vec::new(),
            cache_start: 0,
            checks_left: None,
        };
        body.verify().map_err(Error::Read)?;
        // A new primary transaction sees revocations after the body snapshot.
        let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
        let binding = binding.clone();
        postgres_probe::with_mailbox_client(
            request,
            trust,
            deadline.min(Instant::now() + postgres_probe::MAX_DURATION),
            |client| Box::pin(reauthorize_blob(client, binding, presented, expires)),
        )
        .map_err(Error::Connection)??;
        let mut pin =
            crate::store_fs::PinnedBlob::from_verified(&mut body, metadata.blob, metadata.body)
                .map_err(Error::Read)?;
        let mut timing = crate::performance::Timer::start(crate::performance::Phase::BodyReplay);
        let result = read(metadata, &mut pin);
        if result.is_ok() {
            timing.succeeded();
        }
        drop(timing);
        pin.check_deadline().map_err(Error::Read)?;
        drop(pin);
        body.check().map_err(Error::Read)?;
        result.map_err(Error::Read)
    })
    .map_err(Error::Connection)?
}

async fn blob_snapshot(
    client: &tokio_postgres::Client,
    binding: Binding,
    blob: BlobId,
    maximum: u64,
    presented: Presented,
) -> Result<(BlobMetadata, Option<i64>), Error> {
    let device = presented.id();
    let schema = postgres_schema::lock_and_inspect(client)
        .await
        .map_err(Error::Schema)?;
    if schema.store_epoch != binding.epoch() {
        return Err(Error::Epoch);
    }
    client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)?;
    check_selection(client, &binding, false).await?;
    postgres_devices::authenticate(client, binding.account(), presented)
        .await
        .map_err(Error::Credential)?
        .ok_or(Error::Unauthenticated)?;
    let rows = client.query_typed("SELECT a.sequence,a.floor,b.length,b.digest,b.created_at,b.chunk_count,b.changed,(SELECT count(*) FROM td_mta.blob_chunks c WHERE c.account=b.account AND c.blob_id=b.id),EXISTS(SELECT 1 FROM td_mta.emails e WHERE e.account=b.account AND e.blob_id=b.id) OR EXISTS(SELECT 1 FROM td_mta.submissions s WHERE s.account=b.account AND s.transmitted_blob_id=b.id),l.device_id,l.expires_at,l.changed,(extract(epoch FROM clock_timestamp())*1000)::bigint FROM td_mta.accounts a JOIN td_mta.blobs b ON b.account=a.id LEFT JOIN td_mta.leases l ON l.account=b.account AND l.blob_id=b.id WHERE a.id=$1 AND b.id=$2 LIMIT 2", &[(&binding.account().as_bytes().as_slice(), Type::BYTEA), (&blob.as_bytes().as_slice(), Type::BYTEA)]).await.map_err(|_| Error::Database)?;
    if rows.is_empty() {
        return Err(Error::Missing);
    }
    if rows.len() != 1 {
        return Err(Error::Integrity);
    }
    let row = rows.first().ok_or(Error::Integrity)?;
    let sequence = number(row, 0)?;
    let floor = number(row, 1)?;
    if floor > sequence || number(row, 6)? > sequence {
        return Err(Error::Integrity);
    }
    let referenced: bool = row.try_get(8).map_err(|_| Error::Integrity)?;
    let expires = if referenced {
        None
    } else {
        let owner: Option<&[u8]> = row.try_get(9).map_err(|_| Error::Integrity)?;
        let expires: Option<i64> = row.try_get(10).map_err(|_| Error::Integrity)?;
        let now: i64 = row.try_get(12).map_err(|_| Error::Integrity)?;
        if owner != Some(device.as_bytes().as_slice()) || expires.is_none_or(|expiry| expiry <= now)
        {
            return Err(Error::Missing);
        }
        if number(row, 11)? > sequence {
            return Err(Error::Integrity);
        }
        expires
    };
    Ok((
        blob_metadata(row, binding.account(), binding.epoch(), blob, maximum)?,
        expires,
    ))
}

/// Fixed blob-manifest columns shared by authenticated and relay snapshots.
pub(crate) fn blob_metadata(
    row: &tokio_postgres::Row,
    account: AccountId,
    epoch: StoreEpoch,
    blob: BlobId,
    maximum: u64,
) -> Result<BlobMetadata, Error> {
    let sequence = number(row, 0)?;
    let floor = number(row, 1)?;
    if floor > sequence || number(row, 6)? > sequence {
        return Err(Error::Integrity);
    }
    let length: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
    let length = u64::try_from(length).map_err(|_| Error::Integrity)?;
    if length > MAX_BODY_BYTES {
        return Err(Error::Integrity);
    }
    if length > maximum {
        return Err(Error::Capacity);
    }
    let digest: &[u8] = row.try_get(3).map_err(|_| Error::Integrity)?;
    let chunks: i64 = row.try_get(5).map_err(|_| Error::Integrity)?;
    let actual: i64 = row.try_get(7).map_err(|_| Error::Integrity)?;
    if u64::try_from(chunks).ok() != Some(length.div_ceil(CHUNK)) || chunks != actual {
        return Err(Error::Integrity);
    }
    Ok(BlobMetadata {
        identity: ViewIdentity {
            account,
            epoch,
            committed_sequence: Sequence::from_u64(sequence),
            history_floor: Sequence::from_u64(floor),
        },
        blob,
        body: BlobRow {
            length,
            digest: digest.try_into().map_err(|_| Error::Integrity)?,
            created_at: row.try_get(4).map_err(|_| Error::Integrity)?,
        },
    })
}

async fn reauthorize_blob(
    client: &tokio_postgres::Client,
    binding: Binding,
    presented: Presented,
    expires: Option<i64>,
) -> Result<(), Error> {
    let schema = postgres_schema::lock_and_inspect(client)
        .await
        .map_err(Error::Schema)?;
    if schema.store_epoch != binding.epoch() {
        return Err(Error::Epoch);
    }
    client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|Error::Database)?;
    check_selection(client, &binding, true).await?;
    client
        .query_typed_one(
            "SELECT id FROM td_mta.accounts WHERE id=$1 FOR SHARE",
            &[(&binding.account().as_bytes().as_slice(), Type::BYTEA)],
        )
        .await
        .map_err(|_| Error::Database)?;
    postgres_devices::authenticate(client, binding.account(), presented)
        .await
        .map_err(Error::Credential)?
        .ok_or(Error::Unauthenticated)?;
    let now: i64 = client
        .query_typed_one(
            "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
            &[],
        )
        .await
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Integrity)?;
    if expires.is_some_and(|expiry| expiry <= now) {
        return Err(Error::Missing);
    }
    Ok(())
}

async fn check_selection(
    client: &tokio_postgres::Client,
    binding: &Binding,
    locked: bool,
) -> Result<(), Error> {
    let query = if locked {
        "SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR SHARE"
    } else {
        "SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true"
    };
    let row = client
        .query_typed_one(query, &[])
        .await
        .map_err(|_| Error::Database)?;
    let version: Option<&[u8]> = row.try_get(1).map_err(|_| Error::Integrity)?;
    if number(&row, 0)? != binding.selection().generation
        || version != Some(binding.selection().version.as_bytes().as_slice())
    {
        return Err(Error::Policy);
    }
    Ok(())
}

fn number(row: &tokio_postgres::Row, column: usize) -> Result<u64, Error> {
    let bytes: &[u8] = row.try_get(column).map_err(|_| Error::Integrity)?;
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
fn metadata(row: &tokio_postgres::Row, selected: ReadRequest) -> Result<Metadata, Error> {
    let sequence = number(row, 0)?;
    let floor = number(row, 1)?;
    if floor > sequence
        || (9..=11).any(|column| number(row, column).map_or(true, |changed| changed > sequence))
    {
        return Err(Error::Integrity);
    }
    let blob: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
    let thread: &[u8] = row.try_get(3).map_err(|_| Error::Integrity)?;
    let length: i64 = row.try_get(5).map_err(|_| Error::Integrity)?;
    let length = u64::try_from(length).map_err(|_| Error::Integrity)?;
    if length > MAX_BODY_BYTES {
        return Err(Error::Integrity);
    }
    if length > selected.maximum {
        return Err(Error::Capacity);
    }
    let digest: &[u8] = row.try_get(6).map_err(|_| Error::Integrity)?;
    let chunks: i64 = row.try_get(8).map_err(|_| Error::Integrity)?;
    let actual: i64 = row.try_get(12).map_err(|_| Error::Integrity)?;
    if u64::try_from(chunks).ok() != Some(length.div_ceil(CHUNK)) || actual != chunks {
        return Err(Error::Integrity);
    }
    Ok(Metadata {
        identity: ViewIdentity {
            account: selected.account,
            epoch: selected.epoch,
            committed_sequence: Sequence::from_u64(sequence),
            history_floor: Sequence::from_u64(floor),
        },
        email: selected.email,
        blob: BlobId::from_bytes(blob.try_into().map_err(|_| Error::Integrity)?),
        thread: ThreadId::from_bytes(thread.try_into().map_err(|_| Error::Integrity)?),
        received_at: row.try_get(4).map_err(|_| Error::Integrity)?,
        body: BlobRow {
            length,
            digest: digest.try_into().map_err(|_| Error::Integrity)?,
            created_at: row.try_get(7).map_err(|_| Error::Integrity)?,
        },
    })
}

fn database() -> ports::Error {
    ports::Error::Io {
        kind: std::io::ErrorKind::ConnectionAborted,
        os_code: None,
    }
}
fn connection(error: postgres_probe::Error) -> ports::Error {
    match error {
        postgres_probe::Error::Deadline => ports::Error::Deadline,
        _ => database(),
    }
}

struct Body<'a> {
    worker: &'a mut Worker,
    metadata: BlobMetadata,
    deadline: Instant,
    failed: Option<ports::Error>,
    cache: Vec<tokio_postgres::Row>,
    cache_start: u64,
    checks_left: Option<u8>,
}
impl Body<'_> {
    fn check_cached(&mut self) -> Result<(), ports::Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = match self.checks_left {
            None => self.check(),
            Some(0) => {
                self.checks_left = Some(63);
                self.check()
            }
            Some(left) => {
                self.checks_left = Some(left - 1);
                if Instant::now() >= self.deadline {
                    Err(ports::Error::Deadline)
                } else {
                    self.worker.check_deadline().map_err(connection)
                }
            }
        };
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn check(&mut self) -> Result<(), ports::Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if Instant::now() >= self.deadline {
            self.failed = Some(ports::Error::Deadline);
            return Err(ports::Error::Deadline);
        }
        if let Err(error) = self.worker.check().map_err(connection) {
            self.failed = Some(error);
            return Err(error);
        }
        Ok(())
    }

    fn batch(&mut self, start: u64, end: u64) -> Result<Vec<tokio_postgres::Row>, ports::Error> {
        self.check()?;
        if start >= end || end - start > BATCH || end > self.len().div_ceil(CHUNK) {
            return Err(ports::Error::Invalid);
        }
        let account = self.metadata.identity.account;
        let blob = self.metadata.blob;
        let rows = self.worker.run(|client| Box::pin(async move {
            client.query_typed("SELECT ordinal,bytes FROM td_mta.blob_chunks WHERE account=$1 AND blob_id=$2 AND ordinal>=$3 AND ordinal<$4 ORDER BY ordinal LIMIT 8", &[(&account.as_bytes().as_slice(), Type::BYTEA), (&blob.as_bytes().as_slice(), Type::BYTEA), (&(start as i64), Type::INT8), (&(end as i64), Type::INT8)]).await
        })).map_err(connection)?.map_err(|_| database())?;
        if rows.len() as u64 != end - start {
            return Err(ports::Error::Corrupt);
        }
        for (offset, row) in rows.iter().enumerate() {
            let ordinal: i64 = row.try_get(0).map_err(|_| ports::Error::Corrupt)?;
            let wanted = start + offset as u64;
            let bytes: &[u8] = row.try_get(1).map_err(|_| ports::Error::Corrupt)?;
            if u64::try_from(ordinal).ok() != Some(wanted)
                || bytes.len() as u64 != (self.len() - wanted * CHUNK).min(CHUNK)
            {
                return Err(ports::Error::Corrupt);
            }
        }
        self.check()?;
        Ok(rows)
    }

    fn verify(&mut self) -> Result<(), ports::Error> {
        let mut timing = crate::performance::Timer::start(crate::performance::Phase::BodyVerify);
        let mut digest = Provider.sha256()?;
        let chunks = self.len().div_ceil(CHUNK);
        let mut start = 0;
        while start < chunks {
            let end = (start + BATCH).min(chunks);
            self.cache.clear();
            self.cache = self.batch(start, end)?;
            self.cache_start = start;
            for row in &self.cache {
                let bytes: &[u8] = row.try_get(1).map_err(|_| ports::Error::Corrupt)?;
                digest.update(bytes)?;
            }
            start = end;
        }
        if !Provider.equal_digest(&digest.finish()?, &self.metadata.body.digest) {
            return Err(ports::Error::Corrupt);
        }
        self.check()?;
        timing.succeeded();
        Ok(())
    }

    fn read(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, ports::Error> {
        self.check_cached()?;
        if offset > self.len() {
            return Err(ports::Error::Invalid);
        }
        if output.is_empty() || offset == self.len() {
            return Ok(0);
        }
        let start = offset / CHUNK;
        if start < self.cache_start || start >= self.cache_start + self.cache.len() as u64 {
            let aligned = start / BATCH * BATCH;
            let end = (aligned + BATCH).min(self.len().div_ceil(CHUNK));
            self.cache.clear();
            self.cache = self.batch(aligned, end)?;
            self.cache_start = aligned;
        }
        let mut written = 0;
        let mut skip = (offset % CHUNK) as usize;
        let first = usize::try_from(start - self.cache_start).map_err(|_| ports::Error::Corrupt)?;
        for row in self.cache.get(first..).ok_or(ports::Error::Corrupt)? {
            let bytes: &[u8] = row.try_get(1).map_err(|_| ports::Error::Corrupt)?;
            let bytes = bytes.get(skip..).ok_or(ports::Error::Corrupt)?;
            let take = bytes.len().min(output.len().saturating_sub(written));
            output
                .get_mut(written..written + take)
                .ok_or(ports::Error::Corrupt)?
                .copy_from_slice(bytes.get(..take).ok_or(ports::Error::Corrupt)?);
            written += take;
            skip = 0;
            if written == output.len() {
                break;
            }
        }
        self.check_cached()?;
        Ok(written)
    }
}
impl BlobReader for Body<'_> {
    fn len(&self) -> u64 {
        self.metadata.body.length
    }
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, ports::Error> {
        let result = self.read(offset, output);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
}

impl crate::store_fs::VerifiedSource for Body<'_> {
    fn check(&mut self) -> Result<(), ports::Error> {
        self.check_cached()
    }
}
