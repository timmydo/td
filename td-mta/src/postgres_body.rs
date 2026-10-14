//! Verified Email body reads under one authenticated PostgreSQL snapshot.
use crate::{
    device_credentials::Presented,
    format::{row::BlobRow, Sequence},
    ids::{AccountId, BlobId, EmailId, StoreEpoch, ThreadId},
    ports::{self, BlobReader, ViewIdentity},
    postgres_devices,
    postgres_probe::{self, Request, Worker},
    postgres_schema,
};
use std::time::Instant;
use td_crypto::{Crypto, Digest, Provider, TrustStore};

const CHUNK: u64 = 65536;
const BATCH: u64 = 8;
pub const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Credential(postgres_devices::Error),
    Epoch,
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
            let rows = client.query("SELECT a.sequence,a.floor,e.blob_id,e.thread_id,e.received_at,b.length,b.digest,b.created_at,b.chunk_count,e.changed,b.changed,t.changed,(SELECT count(*) FROM td_mta.blob_chunks c WHERE c.account=a.id AND c.blob_id=e.blob_id) FROM td_mta.accounts a JOIN td_mta.emails e ON e.account=a.id LEFT JOIN td_mta.blobs b ON b.account=e.account AND b.id=e.blob_id LEFT JOIN td_mta.threads t ON t.account=e.account AND t.id=e.thread_id WHERE a.id=$1 AND e.id=$2 LIMIT 2", &[&selected.account.as_bytes().as_slice(), &selected.email.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            if rows.is_empty() { return Err(Error::Missing); }
            if rows.len() != 1 { return Err(Error::Integrity); }
            metadata(rows.first().ok_or(Error::Integrity)?, selected)
        })).map_err(Error::Connection)??;
        let mut body = Body { worker, metadata, deadline, failed: None, cache: Vec::new(), cache_start: 0 };
        body.verify().map_err(Error::Read)?;
        body.check().map_err(Error::Read)?;
        let result = read(metadata, &mut body);
        // Even a callback swallowing a failed read cannot turn it into success.
        body.check().map_err(Error::Read)?;
        result.map_err(Error::Read)
    }).map_err(Error::Connection)?
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
    metadata: Metadata,
    deadline: Instant,
    failed: Option<ports::Error>,
    cache: Vec<tokio_postgres::Row>,
    cache_start: u64,
}
impl Body<'_> {
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
            client.query("SELECT ordinal,bytes FROM td_mta.blob_chunks WHERE account=$1 AND blob_id=$2 AND ordinal>=$3 AND ordinal<$4 ORDER BY ordinal LIMIT 8", &[&account.as_bytes().as_slice(), &blob.as_bytes().as_slice(), &(start as i64), &(end as i64)]).await
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
        self.check()
    }

    fn read(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, ports::Error> {
        self.check()?;
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
        self.check()?;
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
