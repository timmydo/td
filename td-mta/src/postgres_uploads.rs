//! Cold bounded upload publication; runtime ingress reservations remain separate.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    device_credentials::Presented,
    ids::{BlobId, DeviceId, UploadOperationId},
    ports::{Clock, Deadline},
    postgres_devices,
    postgres_probe::{self, Request},
    postgres_publication as publication,
    postgres_runtime_profile::Binding,
    postgres_schema,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use td_crypto::{Crypto, Digest, Entropy, Provider, SystemEntropy, TrustStore};
use tokio_postgres::Client;

pub const MAX_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_RECEIPTS: i64 = 1024;
use publication::CHUNK;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Credential(postgres_devices::Error),
    Epoch,
    Policy,
    Unauthenticated,
    Capacity,
    Quota,
    Integrity,
    Conflict,
    Crypto,
    Database,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub blob: BlobId,
    pub length: u64,
    pub created_at: i64,
    pub expires_at: i64,
    pub old_sequence: u64,
    pub new_sequence: u64,
}

/// Immutable retained input. Clones share one bounded body allocation; no
/// password is retained. Possession does not authorize publication or download.
#[derive(Clone)]
pub struct Upload {
    binding: Binding,
    device: DeviceId,
    operation: UploadOperationId,
    blob: BlobId,
    bytes: Arc<Vec<u8>>,
    body_digest: [u8; 32],
    request_digest: [u8; 32],
    ttl_ms: i64,
}
impl Upload {
    pub fn operation(&self) -> UploadOperationId {
        self.operation
    }
    pub fn blob(&self) -> BlobId {
        self.blob
    }
}

/// Copy already completed ingress, never hold a database transaction while a
/// peer supplies bytes. This cold owner API is not runtime upload admission.
pub fn prepare(
    binding: &Binding,
    device: DeviceId,
    bytes: &[u8],
    deadline: Instant,
) -> Result<Upload, Error> {
    let left = preparation_time(binding, bytes.len(), deadline)?;
    let clock = RuntimeClock::new();
    let now = clock.sample().map_err(|_| Error::Integrity)?.monotonic;
    let cap = Deadline::after(
        now,
        u64::try_from(left.as_millis()).map_err(|_| Error::Capacity)?,
    )
    .map_err(|_| Error::Integrity)?;
    let work = binding.work();
    let mut meter = Meter::new(
        cap,
        Charge {
            io_bytes: work.foreground_io_bytes,
            records: work.foreground_records,
            output_bytes: binding.upload_maximum() as u64,
            ..Charge::default()
        },
    );
    prepare_with_meter(binding, device, bytes, deadline, &mut meter, &clock)
}

fn preparation_time(
    binding: &Binding,
    length: usize,
    deadline: Instant,
) -> Result<std::time::Duration, Error> {
    if !binding.roles().jmap() {
        return Err(Error::Policy);
    }
    if length > MAX_BYTES || length > binding.upload_maximum() {
        return Err(Error::Capacity);
    }
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero() && *left <= postgres_probe::MAX_DURATION)
        .ok_or(Error::Connection(postgres_probe::Error::Deadline))
}

fn charge(
    meter: &mut Meter,
    clock: &RuntimeClock,
    deadline: Instant,
    bytes: usize,
    output: usize,
) -> Result<(), Error> {
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    meter
        .charge(
            clock.sample().map_err(|_| Error::Integrity)?.monotonic,
            Charge {
                io_bytes: bytes as u64,
                output_bytes: output as u64,
                records: 1,
                ..Charge::default()
            },
        )
        .map_err(|error| match error {
            Stop::Deadline => Error::Connection(postgres_probe::Error::Deadline),
            _ => Error::Capacity,
        })
}

/// Retain completed ingress under its original work allowance and deadline.
/// The caller still owns memory/concurrency admission; this grants no device
/// or publication authority and never opens a database transaction.
pub fn prepare_with_meter(
    binding: &Binding,
    device: DeviceId,
    bytes: &[u8],
    deadline: Instant,
    meter: &mut Meter,
    clock: &RuntimeClock,
) -> Result<Upload, Error> {
    preparation_time(binding, bytes.len(), deadline)?;
    charge(meter, clock, deadline, 0, bytes.len())?;
    let ttl_ms = i64::try_from(binding.upload_expiry_seconds())
        .ok()
        .and_then(|n| n.checked_mul(1000))
        .ok_or(Error::Integrity)?;
    let mut body = Provider.sha256().map_err(|_| Error::Crypto)?;
    for chunk in bytes.chunks(CHUNK) {
        charge(meter, clock, deadline, chunk.len(), 0)?;
        body.update(chunk).map_err(|_| Error::Crypto)?;
    }
    let body_digest = body.finish().map_err(|_| Error::Crypto)?;
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|_| Error::Capacity)?;
    for chunk in bytes.chunks(CHUNK) {
        charge(meter, clock, deadline, chunk.len(), 0)?;
        owned.extend_from_slice(chunk);
    }
    let mut rng = SystemEntropy::try_new().map_err(|_| Error::Crypto)?;
    let mut operation = [0; 16];
    let mut blob = [0; 16];
    rng.fill(&mut operation).map_err(|_| Error::Crypto)?;
    rng.fill(&mut blob).map_err(|_| Error::Crypto)?;
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    let selected = binding.selection();
    for part in [
        b"td-mta PostgreSQL upload v1\0".as_slice(),
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        &selected.generation.to_be_bytes(),
        selected.version.as_bytes().as_slice(),
        device.as_bytes().as_slice(),
        &blob,
        &(bytes.len() as u64).to_be_bytes(),
        &body_digest,
        &ttl_ms.to_be_bytes(),
        binding.profile().as_bytes(),
    ] {
        charge(meter, clock, deadline, part.len(), 0)?;
        hash.update(part).map_err(|_| Error::Crypto)?;
    }
    charge(meter, clock, deadline, 0, 0)?;
    Ok(Upload {
        binding: binding.clone(),
        device,
        operation: UploadOperationId::from_bytes(operation),
        blob: BlobId::from_bytes(blob),
        bytes: Arc::new(owned),
        body_digest,
        request_digest: hash.finish().map_err(|_| Error::Crypto)?,
        ttl_ms,
    })
}

fn sequence(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
async fn begin(client: &Client, upload: &Upload) -> Result<(), Error> {
    let schema = postgres_schema::lock_and_inspect(client)
        .await
        .map_err(Error::Schema)?;
    if schema.store_epoch != upload.binding.epoch() {
        return Err(Error::Epoch);
    }
    client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)
}
async fn receipt(client: &Client, upload: &Upload) -> Result<Option<Outcome>, Error> {
    let rows = client.query("SELECT request_digest,blob_id,length,created_at,expires_at,old_sequence,new_sequence FROM td_mta.upload_receipts WHERE account=$1 AND operation=$2 LIMIT 2", &[&upload.binding.account().as_bytes().as_slice(), &upload.operation.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 {
        return Err(Error::Integrity);
    }
    let row = rows.first().ok_or(Error::Integrity)?;
    let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if digest != upload.request_digest {
        return Err(Error::Conflict);
    }
    let blob: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
    let length: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
    let created_at: i64 = row.try_get(3).map_err(|_| Error::Integrity)?;
    let expires_at: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
    let old_sequence = sequence(row.try_get(5).map_err(|_| Error::Integrity)?)?;
    let new_sequence = sequence(row.try_get(6).map_err(|_| Error::Integrity)?)?;
    if blob != upload.blob.as_bytes()
        || usize::try_from(length).ok() != Some(upload.bytes.len())
        || created_at.checked_add(upload.ttl_ms) != Some(expires_at)
        || old_sequence.checked_add(1) != Some(new_sequence)
    {
        return Err(Error::Integrity);
    }
    Ok(Some(Outcome {
        blob: upload.blob,
        length: length as u64,
        created_at,
        expires_at,
        old_sequence,
        new_sequence,
    }))
}

/// A primary account lock excludes an earlier publication still committing.
/// A known receipt remains success after revocation, expiry or policy changes.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    upload: &Upload,
) -> Result<Option<Outcome>, Error> {
    let upload = upload.clone();
    let result = postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &upload).await?;
            client
                .query_one(
                    "SELECT id FROM td_mta.accounts WHERE id=$1 FOR SHARE",
                    &[&upload.binding.account().as_bytes().as_slice()],
                )
                .await
                .map_err(|_| Error::Database)?;
            receipt(client, &upload).await
        })
    })
    .map_err(Error::Connection)?;
    if !matches!(result, Ok(Some(_))) && Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

/// Publish the immutable body, lease, quota charges, account state and receipt
/// together. On uncertainty retain Upload and resolve this same operation.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    upload: &Upload,
    password: &[u8],
) -> Result<Outcome, Error> {
    if let Some(outcome) = inspect(request, trust, deadline, upload)? {
        return Ok(outcome);
    }
    let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
    if presented.id() != upload.device {
        return Err(Error::Unauthenticated);
    }
    let upload = upload.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_upload_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &upload).await?;
            let selected = client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR SHARE", &[]).await.map_err(|_| Error::Database)?;
            let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
            let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
            let account = upload.binding.account();
            let account = account.as_bytes().as_slice();
            let state = client
                .query_one(
                    "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
                    &[&account],
                )
                .await
                .map_err(|_| Error::Database)?;
            if let Some(outcome) = receipt(client, &upload).await? {
                return Ok(outcome);
            }
            if generation != upload.binding.selection().generation
                || version != Some(upload.binding.selection().version.as_bytes().as_slice())
                || !upload.binding.roles().jmap()
            {
                return Err(Error::Policy);
            }
            if postgres_devices::authenticate(client, upload.binding.account(), presented)
                .await
                .map_err(Error::Credential)?
                != Some(upload.device)
            {
                return Err(Error::Unauthenticated);
            }
            let old = sequence(state.try_get(0).map_err(|_| Error::Integrity)?)?;
            let floor = sequence(state.try_get(1).map_err(|_| Error::Integrity)?)?;
            if floor > old {
                return Err(Error::Integrity);
            }
            let next = crate::postgres_sequence::next(client, account, old)
                .await
                .map_err(|error| match error {
                    crate::postgres_sequence::Error::Capacity => Error::Capacity,
                    crate::postgres_sequence::Error::Integrity => Error::Integrity,
                    crate::postgres_sequence::Error::Database => Error::Database,
                })?;
            let count: i64 = client.query_one("SELECT count(*) FROM (SELECT 1 FROM td_mta.upload_receipts WHERE account=$1 LIMIT $2) bounded", &[&account, &MAX_RECEIPTS]).await.map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Integrity)?;
            if count >= MAX_RECEIPTS {
                return Err(Error::Capacity);
            }
            let usage = publication::charge(
                client,
                upload.binding.account(),
                upload.binding.quotas(),
                [
                    upload.bytes.len() as u64,
                    1,
                    upload.bytes.len() as u64,
                    0,
                    0,
                ],
            )
            .await
            .map_err(publication_error)?;
            let created_at: i64 = client
                .query_one(
                    "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
                    &[],
                )
                .await
                .map_err(|_| Error::Database)?
                .try_get(0)
                .map_err(|_| Error::Integrity)?;
            let expires_at = created_at
                .checked_add(upload.ttl_ms)
                .ok_or(Error::Integrity)?;
            let blob = upload.blob.as_bytes().as_slice();
            let length = i64::try_from(upload.bytes.len()).map_err(|_| Error::Capacity)?;
            publication::write_blob(
                client,
                publication::Blob {
                    account: upload.binding.account(),
                    id: upload.blob,
                    bytes: &upload.bytes,
                    digest: &upload.body_digest,
                    created_at,
                    changed: next,
                },
                deadline,
            )
            .await
            .map_err(publication_error)?;
            if client
                .execute(
                    "INSERT INTO td_mta.leases VALUES($1,$2,$3,$4,3,$5)",
                    &[
                        &account,
                        &blob,
                        &upload.device.as_bytes().as_slice(),
                        &expires_at,
                        &next.to_be_bytes().as_slice(),
                    ],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            usage
                .publish(client, upload.binding.account())
                .await
                .map_err(publication_error)?;
            if client
                .execute(
                    "UPDATE td_mta.accounts SET sequence=$2 WHERE id=$1",
                    &[&account, &next.to_be_bytes().as_slice()],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            if client
                .execute(
                    "INSERT INTO td_mta.upload_receipts VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)",
                    &[
                        &account,
                        &upload.operation.as_bytes().as_slice(),
                        &upload.request_digest.as_slice(),
                        &blob,
                        &length,
                        &created_at,
                        &expires_at,
                        &old.to_be_bytes().as_slice(),
                        &next.to_be_bytes().as_slice(),
                    ],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(Outcome {
                blob: upload.blob,
                length: length as u64,
                created_at,
                expires_at,
                old_sequence: old,
                new_sequence: next,
            })
        })
    });
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}

fn publication_error(error: publication::Error) -> Error {
    match error {
        publication::Error::Capacity => Error::Capacity,
        publication::Error::Quota => Error::Quota,
        publication::Error::Integrity => Error::Integrity,
        publication::Error::Conflict => Error::Conflict,
        publication::Error::Database => Error::Database,
        publication::Error::Deadline => Error::Connection(postgres_probe::Error::Deadline),
    }
}
