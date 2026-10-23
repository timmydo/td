//! Retained atomic publication of prepared td-mail submissions.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    device_credentials::Presented,
    ids::{BlobId, EmailId, SubmissionId, SubmissionOperationId, ThreadId},
    ports::{self, Clock, Deadline},
    postgres_devices,
    postgres_probe::{self, Request},
    postgres_publication as publication, postgres_schema,
    postgres_submission_input::Prepared,
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

pub const MAX_RECEIPTS: i64 = 1024;
const LIFETIME_MS: i64 = 5 * 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Credential(postgres_devices::Error),
    Work(ports::Error),
    Epoch,
    Policy,
    Unauthenticated,
    Capacity,
    Quota,
    Integrity,
    Conflict,
    State,
    Source,
    Crypto,
    Database,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub submission: SubmissionId,
    pub email: EmailId,
    pub thread: ThreadId,
    pub blob: BlobId,
    pub length: u64,
    pub send_at: i64,
    pub expires_at: i64,
    pub old_sequence: u64,
    pub new_sequence: u64,
}

/// Keep the same operation and immutable input across uncertain COMMIT.
#[derive(Clone)]
pub struct Submission {
    input: Prepared,
    operation: SubmissionOperationId,
    id: SubmissionId,
    blob: BlobId,
    body_digest: [u8; 32],
    request_digest: [u8; 32],
}
impl Submission {
    pub fn route(&self) -> &crate::postgres_routes::Route {
        self.input.binding().submission_route()
    }
    pub fn operation(&self) -> SubmissionOperationId {
        self.operation
    }
    pub fn id(&self) -> SubmissionId {
        self.id
    }
    pub fn blob(&self) -> BlobId {
        self.blob
    }
}

pub fn prepare(input: Prepared, deadline: Instant) -> Result<Submission, Error> {
    check_deadline(deadline)?;
    let clock = RuntimeClock::new();
    let left = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Error::Connection(postgres_probe::Error::Deadline))?;
    let cap = Deadline::after(
        clock.sample().map_err(Error::Work)?.monotonic,
        u64::try_from(left.as_millis()).map_err(|_| Error::Capacity)?,
    )
    .map_err(Error::Work)?;
    let work = input.binding().work();
    let mut meter = Meter::new(
        cap,
        Charge {
            io_bytes: work.foreground_io_bytes,
            records: work.foreground_records,
            ..Charge::default()
        },
    );
    prepare_with_meter(input, deadline, &mut meter, &clock)
}

/// Hash the retained input under the original request budget. Publication and
/// retained-memory admission remain separate obligations of the caller.
pub fn prepare_with_meter(
    input: Prepared,
    deadline: Instant,
    meter: &mut Meter,
    clock: &RuntimeClock,
) -> Result<Submission, Error> {
    charge(clock, meter, deadline, 0)?;
    let mut entropy = SystemEntropy::try_new().map_err(|_| Error::Crypto)?;
    let mut ids = [[0; 16]; 3];
    for id in &mut ids {
        entropy.fill(id).map_err(|_| Error::Crypto)?;
    }
    let [operation, id, blob] = ids;
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for chunk in input.bytes().chunks(publication::CHUNK) {
        charge(clock, meter, deadline, chunk.len())?;
        hash.update(chunk).map_err(|_| Error::Crypto)?;
    }
    let body_digest = hash.finish().map_err(|_| Error::Crypto)?;
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    let binding = input.binding();
    let selection = binding.selection();
    let source = input.source();
    for bytes in [
        b"td-mta PostgreSQL submission v2".as_slice(),
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        &selection.generation.to_be_bytes(),
        selection.version.as_bytes().as_slice(),
        input.device().as_bytes().as_slice(),
        input.identity().as_bytes().as_slice(),
        source.email.as_bytes().as_slice(),
        source.blob.as_bytes().as_slice(),
        source.thread.as_bytes().as_slice(),
        &source.body.length.to_be_bytes(),
        &source.body.digest,
        &source.identity.committed_sequence.number().to_be_bytes(),
        &operation,
        &id,
        &blob,
        &(input.bytes().len() as u64).to_be_bytes(),
        &body_digest,
        binding.submission_route().destination(),
        &[u8::from(input.expected().is_some())],
        &input.expected().unwrap_or(0).to_be_bytes(),
        &(input.recipients().len() as u64).to_be_bytes(),
    ] {
        charge(clock, meter, deadline, bytes.len())?;
        hash.update(bytes).map_err(|_| Error::Crypto)?;
    }
    for text in [
        binding.profile(),
        binding.submission_route().name(),
        input.reverse_path(),
    ]
    .into_iter()
    .chain(input.recipients())
    {
        charge(
            clock,
            meter,
            deadline,
            text.len().checked_add(8).ok_or(Error::Capacity)?,
        )?;
        hash.update(&(text.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(text.as_bytes()).map_err(|_| Error::Crypto)?;
    }
    let request_digest = hash.finish().map_err(|_| Error::Crypto)?;
    charge(clock, meter, deadline, 0)?;
    Ok(Submission {
        input,
        operation: SubmissionOperationId::from_bytes(operation),
        id: SubmissionId::from_bytes(id),
        blob: BlobId::from_bytes(blob),
        body_digest,
        request_digest,
    })
}
fn charge(
    clock: &RuntimeClock,
    meter: &mut Meter,
    deadline: Instant,
    bytes: usize,
) -> Result<(), Error> {
    check_deadline(deadline)?;
    meter
        .charge(
            clock.sample().map_err(Error::Work)?.monotonic,
            Charge {
                io_bytes: bytes as u64,
                records: 1,
                ..Charge::default()
            },
        )
        .map_err(|error| {
            Error::Work(match error {
                Stop::Deadline => ports::Error::Deadline,
                _ => ports::Error::Capacity,
            })
        })
}
fn check_deadline(deadline: Instant) -> Result<(), Error> {
    if deadline
        .checked_duration_since(Instant::now())
        .is_none_or(|left| left.is_zero() || left > postgres_probe::MAX_DURATION)
    {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    Ok(())
}
fn sequence(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
async fn begin(client: &Client, value: &Submission) -> Result<(), Error> {
    let schema = postgres_schema::lock_and_inspect(client)
        .await
        .map_err(Error::Schema)?;
    if schema.store_epoch != value.input.binding().epoch() {
        return Err(Error::Epoch);
    }
    client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000")
        .await.map_err(|_| Error::Database)
}
async fn receipt(client: &Client, value: &Submission) -> Result<Option<Outcome>, Error> {
    let rows = client.query(
        "SELECT request_digest,submission_id,blob_id,length,send_at,expires_at,old_sequence,new_sequence FROM td_mta.submission_receipts WHERE account=$1 AND operation=$2 LIMIT 2",
        &[&value.input.binding().account().as_bytes().as_slice(), &value.operation.as_bytes().as_slice()]
    ).await.map_err(|_| Error::Database)?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 {
        return Err(Error::Integrity);
    }
    let row = rows.first().ok_or(Error::Integrity)?;
    let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if digest != value.request_digest {
        return Err(Error::Conflict);
    }
    let submission: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
    let blob: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
    let length: i64 = row.try_get(3).map_err(|_| Error::Integrity)?;
    let send_at: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
    let expires_at: i64 = row.try_get(5).map_err(|_| Error::Integrity)?;
    let old = sequence(row.try_get(6).map_err(|_| Error::Integrity)?)?;
    let next = sequence(row.try_get(7).map_err(|_| Error::Integrity)?)?;
    if submission != value.id.as_bytes()
        || blob != value.blob.as_bytes()
        || usize::try_from(length).ok() != Some(value.input.bytes().len())
        || send_at.checked_add(LIFETIME_MS) != Some(expires_at)
        || old.checked_add(1) != Some(next)
        || value
            .input
            .expected()
            .is_some_and(|expected| expected != old)
    {
        return Err(Error::Integrity);
    }
    let source = value.input.source();
    Ok(Some(Outcome {
        submission: value.id,
        email: source.email,
        thread: source.thread,
        blob: value.blob,
        length: length as u64,
        send_at,
        expires_at,
        old_sequence: old,
        new_sequence: next,
    }))
}

pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Submission,
) -> Result<Option<Outcome>, Error> {
    let value = value.clone();
    let result = postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &value).await?;
            client
                .query_one(
                    "SELECT id FROM td_mta.accounts WHERE id=$1 FOR SHARE",
                    &[&value.input.binding().account().as_bytes().as_slice()],
                )
                .await
                .map_err(|_| Error::Database)?;
            receipt(client, &value).await
        })
    })
    .map_err(Error::Connection)?;
    if !matches!(result, Ok(Some(_))) {
        check_deadline(deadline)?;
    }
    result
}

/// Known historical success does not require current device authorization.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Submission,
    password: &[u8],
) -> Result<Outcome, Error> {
    if let Some(outcome) = inspect(request, trust, deadline, value)? {
        return Ok(outcome);
    }
    let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
    if presented.id() != value.input.device() {
        return Err(Error::Unauthenticated);
    }
    let value = value.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_upload_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &value).await?;
            let selected = client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR SHARE", &[])
                .await.map_err(|_| Error::Database)?;
            let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
            let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
            let input = &value.input;
            let binding = input.binding();
            let account_id = binding.account();
            let account = account_id.as_bytes().as_slice();
            let state = client
                .query_one(
                    "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
                    &[&account],
                )
                .await
                .map_err(|_| Error::Database)?;
            if let Some(outcome) = receipt(client, &value).await? {
                return Ok(outcome);
            }
            if generation != binding.selection().generation
                || version != Some(binding.selection().version.as_bytes().as_slice())
                || !binding.roles().jmap()
                || binding.identity_email(input.identity()).is_none()
            {
                return Err(Error::Policy);
            }
            if postgres_devices::authenticate(client, account_id, presented)
                .await
                .map_err(Error::Credential)?
                != Some(input.device())
            {
                return Err(Error::Unauthenticated);
            }
            let old = sequence(state.try_get(0).map_err(|_| Error::Integrity)?)?;
            if sequence(state.try_get(1).map_err(|_| Error::Integrity)?)? > old {
                return Err(Error::Integrity);
            }
            if input.expected().is_some_and(|expected| expected != old) {
                return Err(Error::State);
            }
            let next = crate::postgres_sequence::next(client, account, old)
                .await
                .map_err(|error| match error {
                    crate::postgres_sequence::Error::Capacity => Error::Capacity,
                    crate::postgres_sequence::Error::Integrity => Error::Integrity,
                    crate::postgres_sequence::Error::Database => Error::Database,
                })?;
            let count: i64 = client.query_one("SELECT count(*) FROM (SELECT 1 FROM td_mta.submission_receipts WHERE account=$1 LIMIT $2) bounded",
                &[&account, &MAX_RECEIPTS]).await.map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Integrity)?;
            if count >= MAX_RECEIPTS {
                return Err(Error::Capacity);
            }
            let source = input.source();
            let rows = client.query("SELECT e.blob_id,e.thread_id,e.changed,b.length,b.digest,b.changed FROM td_mta.emails e LEFT JOIN td_mta.blobs b ON b.account=e.account AND b.id=e.blob_id WHERE e.account=$1 AND e.id=$2 LIMIT 1",
                &[&account, &source.email.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            let row = rows.first().ok_or(Error::Source)?;
            let blob: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
            let thread: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
            let length: i64 = row.try_get(3).map_err(|_| Error::Integrity)?;
            let digest: &[u8] = row.try_get(4).map_err(|_| Error::Integrity)?;
            if blob != source.blob.as_bytes()
                || thread != source.thread.as_bytes()
                || u64::try_from(length).ok() != Some(source.body.length)
                || digest != source.body.digest
            {
                return Err(Error::Source);
            }
            if sequence(row.try_get(2).map_err(|_| Error::Integrity)?)? > old
                || sequence(row.try_get(5).map_err(|_| Error::Integrity)?)? > old
            {
                return Err(Error::Integrity);
            }
            let submission = value.id.as_bytes().as_slice();
            let used: bool = client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.submissions WHERE account=$1 AND id=$2) OR EXISTS(SELECT 1 FROM td_mta.changes WHERE account=$1 AND kind=5 AND object=$2)",
                &[&account, &submission]).await.map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Integrity)?;
            if used {
                return Err(Error::Conflict);
            }
            let length = input.bytes().len() as u64;
            let usage = publication::charge(
                client,
                account_id,
                binding.quotas(),
                [length, 1, 0, length, 1],
            )
            .await
            .map_err(publication_error)?;
            let send_at: i64 = client
                .query_one(
                    "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
                    &[],
                )
                .await
                .map_err(|_| Error::Database)?
                .try_get(0)
                .map_err(|_| Error::Integrity)?;
            let expires_at = send_at.checked_add(LIFETIME_MS).ok_or(Error::Capacity)?;
            publication::write_blob(
                client,
                publication::Blob {
                    account: account_id,
                    id: value.blob,
                    bytes: input.bytes(),
                    digest: &value.body_digest,
                    created_at: send_at,
                    changed: next,
                },
                deadline,
            )
            .await
            .map_err(publication_error)?;
            let changed = next.to_be_bytes();
            let changed = changed.as_slice();
            let count = i64::try_from(input.recipients().len()).map_err(|_| Error::Capacity)?;
            if client.execute("INSERT INTO td_mta.submissions(account,id,email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,recipient_count,notification,changed) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,0,$11)",
                &[&account, &submission, &source.email.as_bytes().as_slice(),
                    &source.thread.as_bytes().as_slice(), &input.identity().as_bytes().as_slice(),
                    &value.blob.as_bytes().as_slice(), &input.reverse_path(), &send_at, &expires_at, &count, &changed])
                .await.map_err(|_| Error::Database)? != 1 { return Err(Error::Integrity); }
            if client.execute("INSERT INTO td_mta.submission_routes(account,submission_id,submitting_profile,route_name,destination) VALUES($1,$2,$3,$4,$5)",
                &[&account, &submission, &binding.profile(), &binding.submission_route().name(), &binding.submission_route().destination().as_slice()])
                .await.map_err(|_| Error::Database)? != 1 { return Err(Error::Integrity); }
            let mut recipients = Vec::new();
            recipients
                .try_reserve_exact(input.recipients().len())
                .map_err(|_| Error::Capacity)?;
            recipients.extend(input.recipients());
            if client.execute("INSERT INTO td_mta.recipients(account,submission_id,ordinal,address,state,uncertain,attempt_count,phase,next_attempt_at,reason,diagnostic,changed) SELECT $1,$2,ordinality-1,address,1,0,0,0,$4,0,'',$5 FROM unnest($3::text[]) WITH ORDINALITY recipients(address,ordinality)",
                &[&account, &submission, &recipients, &send_at, &changed]).await.map_err(|_| Error::Database)? != count as u64 {
                return Err(Error::Integrity);
            }
            if client
                .execute(
                    "INSERT INTO td_mta.changes VALUES($1,$2,0,5,1,$3)",
                    &[&account, &changed, &submission],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            usage
                .publish(client, account_id)
                .await
                .map_err(publication_error)?;
            if client
                .execute(
                    "UPDATE td_mta.accounts SET sequence=$2 WHERE id=$1",
                    &[&account, &changed],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            if client
                .execute(
                    "INSERT INTO td_mta.submission_receipts VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
                    &[
                        &account,
                        &value.operation.as_bytes().as_slice(),
                        &value.request_digest.as_slice(),
                        &submission,
                        &value.blob.as_bytes().as_slice(),
                        &(length as i64),
                        &send_at,
                        &expires_at,
                        &old.to_be_bytes().as_slice(),
                        &changed,
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
                submission: value.id,
                email: source.email,
                thread: source.thread,
                blob: value.blob,
                length,
                send_at,
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
