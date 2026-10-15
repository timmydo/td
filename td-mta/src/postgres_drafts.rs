//! Retained plain-text draft creation for td-mail; no submission authority.
use crate::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    compose_text::{self, Message},
    device_credentials::Presented,
    ids::{BlobId, DeviceId, DraftOperationId, EmailId, MailboxId, ThreadId},
    ports::{self, Clock, Deadline},
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

pub const MAX_RECEIPTS: i64 = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Credential(postgres_devices::Error),
    Compose(ports::Error),
    Epoch,
    Policy,
    Unauthenticated,
    Capacity,
    Quota,
    Integrity,
    Conflict,
    State,
    MissingMailbox,
    Crypto,
    Database,
    Indeterminate,
}

pub struct DraftRequest<'a> {
    pub binding: &'a Binding,
    pub device: DeviceId,
    pub mailbox: MailboxId,
    pub expected: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub email: EmailId,
    pub blob: BlobId,
    pub thread: ThreadId,
    pub mailbox: MailboxId,
    pub length: u64,
    pub created_at: i64,
    pub old_sequence: u64,
    pub new_sequence: u64,
}

/// Retain the same operation after uncertainty. No app password is retained.
#[derive(Clone)]
pub struct Draft {
    binding: Binding,
    device: DeviceId,
    mailbox: MailboxId,
    expected: Option<u64>,
    operation: DraftOperationId,
    email: EmailId,
    blob: BlobId,
    new_thread: ThreadId,
    bytes: Arc<Vec<u8>>,
    body_digest: [u8; 32],
    request_digest: [u8; 32],
    message_id: String,
    anchors: Vec<String>,
}
impl Draft {
    pub fn operation(&self) -> DraftOperationId {
        self.operation
    }
    pub fn email(&self) -> EmailId {
        self.email
    }
    pub fn blob(&self) -> BlobId {
        self.blob
    }
}

fn copy(value: &str) -> Result<String, Error> {
    let mut result = String::new();
    result
        .try_reserve_exact(value.len())
        .map_err(|_| Error::Capacity)?;
    result.push_str(value);
    Ok(result)
}

/// Compose outside the database transaction, using the existing td-mail
/// composer and the selected profile's bounded work and message budgets.
pub fn prepare(
    request: DraftRequest<'_>,
    message: &Message<'_>,
    deadline: Instant,
) -> Result<Draft, Error> {
    let left = preparation_time(request.binding, message, deadline)?;
    let maximum = request.binding.upload_maximum().min(32 * 1024 * 1024);
    let clock = RuntimeClock::new();
    let now = clock.sample().map_err(Error::Compose)?;
    let cap = Deadline::after(
        now.monotonic,
        u64::try_from(left.as_millis()).map_err(|_| Error::Capacity)?,
    )
    .map_err(Error::Compose)?;
    let work = request.binding.work();
    let mut meter = Meter::new(
        cap,
        Charge {
            io_bytes: work.foreground_io_bytes,
            records: work.foreground_records,
            output_bytes: maximum as u64,
            ..Charge::default()
        },
    );
    prepare_with_meter(request, message, deadline, &mut meter, &clock)
}

fn preparation_time(
    binding: &Binding,
    message: &Message<'_>,
    deadline: Instant,
) -> Result<std::time::Duration, Error> {
    if !binding.roles().jmap() {
        return Err(Error::Policy);
    }
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero() && *left <= postgres_probe::MAX_DURATION)
        .ok_or(Error::Connection(postgres_probe::Error::Deadline))?;
    if message.text.len() > binding.upload_maximum().min(32 * 1024 * 1024) {
        return Err(Error::Capacity);
    }
    Ok(left)
}
struct PreparationClock<'a> {
    clock: &'a RuntimeClock,
    deadline: Instant,
}
impl Clock for PreparationClock<'_> {
    fn sample(&self) -> Result<ports::Time, ports::Error> {
        if Instant::now() >= self.deadline {
            return Err(ports::Error::Deadline);
        }
        self.clock.sample()
    }
}
fn charge(clock: &impl Clock, meter: &mut Meter, bytes: usize) -> Result<(), Error> {
    meter
        .charge(
            clock.sample().map_err(Error::Compose)?.monotonic,
            Charge {
                io_bytes: bytes as u64,
                records: 1,
                ..Charge::default()
            },
        )
        .map_err(|error| {
            Error::Compose(match error {
                crate::admission::work::Stop::Deadline => ports::Error::Deadline,
                _ => ports::Error::Capacity,
            })
        })
}

/// Compose and retain under the caller's original work allowance. This still
/// requires external memory admission and grants no publication authority.
pub fn prepare_with_meter(
    request: DraftRequest<'_>,
    message: &Message<'_>,
    deadline: Instant,
    meter: &mut Meter,
    clock: &RuntimeClock,
) -> Result<Draft, Error> {
    preparation_time(request.binding, message, deadline)?;
    let binding = request.binding;
    let maximum = binding.upload_maximum().min(32 * 1024 * 1024);
    let clock = PreparationClock { clock, deadline };
    charge(&clock, meter, 0)?;
    let mut bytes = Vec::new();
    let length = compose_text::compose(message, maximum as u64, &clock, meter, |part| {
        bytes
            .try_reserve(part.len())
            .map_err(|_| ports::Error::Capacity)?;
        bytes.extend_from_slice(part);
        Ok(())
    })
    .map_err(Error::Compose)?;
    if length != bytes.len() as u64 {
        return Err(Error::Integrity);
    }
    let mut digest = Provider.sha256().map_err(|_| Error::Crypto)?;
    for chunk in bytes.chunks(publication::CHUNK) {
        charge(&clock, meter, chunk.len())?;
        if Instant::now() >= deadline {
            return Err(Error::Connection(postgres_probe::Error::Deadline));
        }
        digest.update(chunk).map_err(|_| Error::Crypto)?;
    }
    let body_digest = digest.finish().map_err(|_| Error::Crypto)?;
    let mut anchors = Vec::new();
    anchors.try_reserve_exact(65).map_err(|_| Error::Capacity)?;
    for id in message
        .references
        .iter()
        .rev()
        .take(32)
        .chain(message.in_reply_to.iter().rev().take(32))
        .copied()
        .chain(std::iter::once(message.message_id))
    {
        charge(&clock, meter, id.len())?;
        anchors.push(copy(id)?);
    }
    let mut ids = [[0; 16]; 4];
    let mut entropy = SystemEntropy::try_new().map_err(|_| Error::Crypto)?;
    for id in &mut ids {
        entropy.fill(id).map_err(|_| Error::Crypto)?;
    }
    let [operation, email, blob, new_thread] = ids;
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    let selected = binding.selection();
    for part in [
        b"td-mta PostgreSQL draft v1\0".as_slice(),
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        &selected.generation.to_be_bytes(),
        selected.version.as_bytes().as_slice(),
        request.device.as_bytes().as_slice(),
        request.mailbox.as_bytes().as_slice(),
        &email,
        &blob,
        &new_thread,
        &length.to_be_bytes(),
        &body_digest,
        &[u8::from(request.expected.is_some())],
        &request.expected.unwrap_or(0).to_be_bytes(),
        binding.profile().as_bytes(),
    ] {
        charge(&clock, meter, part.len())?;
        hash.update(part).map_err(|_| Error::Crypto)?;
    }
    let request_digest = hash.finish().map_err(|_| Error::Crypto)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    charge(&clock, meter, message.message_id.len())?;
    Ok(Draft {
        binding: binding.clone(),
        device: request.device,
        mailbox: request.mailbox,
        expected: request.expected,
        operation: DraftOperationId::from_bytes(operation),
        email: EmailId::from_bytes(email),
        blob: BlobId::from_bytes(blob),
        new_thread: ThreadId::from_bytes(new_thread),
        bytes: Arc::new(bytes),
        body_digest,
        request_digest,
        message_id: copy(message.message_id)?,
        anchors,
    })
}

fn sequence(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
async fn begin(client: &Client, draft: &Draft) -> Result<(), Error> {
    let schema = postgres_schema::lock_and_inspect(client)
        .await
        .map_err(Error::Schema)?;
    if schema.store_epoch != draft.binding.epoch() {
        return Err(Error::Epoch);
    }
    client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|Error::Database)
}
async fn receipt(client: &Client, draft: &Draft) -> Result<Option<Outcome>, Error> {
    let rows=client.query("SELECT request_digest,email_id,blob_id,thread_id,mailbox_id,length,created_at,old_sequence,new_sequence FROM td_mta.draft_receipts WHERE account=$1 AND operation=$2 LIMIT 2", &[&draft.binding.account().as_bytes().as_slice(),&draft.operation.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 {
        return Err(Error::Integrity);
    }
    let row = rows.first().ok_or(Error::Integrity)?;
    let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if digest != draft.request_digest {
        return Err(Error::Conflict);
    }
    let email: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
    let blob: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
    let thread: &[u8] = row.try_get(3).map_err(|_| Error::Integrity)?;
    let mailbox: &[u8] = row.try_get(4).map_err(|_| Error::Integrity)?;
    let length: i64 = row.try_get(5).map_err(|_| Error::Integrity)?;
    let old = sequence(row.try_get(7).map_err(|_| Error::Integrity)?)?;
    let next = sequence(row.try_get(8).map_err(|_| Error::Integrity)?)?;
    if email != draft.email.as_bytes()
        || blob != draft.blob.as_bytes()
        || mailbox != draft.mailbox.as_bytes()
        || usize::try_from(length).ok() != Some(draft.bytes.len())
        || old.checked_add(1) != Some(next)
        || draft.expected.is_some_and(|expected| expected != old)
    {
        return Err(Error::Integrity);
    }
    Ok(Some(Outcome {
        email: draft.email,
        blob: draft.blob,
        thread: ThreadId::from_bytes(thread.try_into().map_err(|_| Error::Integrity)?),
        mailbox: draft.mailbox,
        length: length as u64,
        created_at: row.try_get(6).map_err(|_| Error::Integrity)?,
        old_sequence: old,
        new_sequence: next,
    }))
}

pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    draft: &Draft,
) -> Result<Option<Outcome>, Error> {
    let draft = draft.clone();
    let result = postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &draft).await?;
            client
                .query_one(
                    "SELECT id FROM td_mta.accounts WHERE id=$1 FOR SHARE",
                    &[&draft.binding.account().as_bytes().as_slice()],
                )
                .await
                .map_err(|_| Error::Database)?;
            receipt(client, &draft).await
        })
    })
    .map_err(Error::Connection)?;
    if !matches!(result, Ok(Some(_))) && Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

/// A historical receipt is known success, not authority for a fresh write.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    draft: &Draft,
    password: &[u8],
) -> Result<Outcome, Error> {
    if let Some(outcome) = inspect(request, trust, deadline, draft)? {
        return Ok(outcome);
    }
    let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
    if presented.id() != draft.device {
        return Err(Error::Unauthenticated);
    }
    let draft = draft.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_upload_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &draft).await?;
            let selected=client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR SHARE",&[]).await.map_err(|_|Error::Database)?;
            let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
            let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
            let account_id = draft.binding.account();
            let account = account_id.as_bytes().as_slice();
            let state = client
                .query_one(
                    "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
                    &[&account],
                )
                .await
                .map_err(|_| Error::Database)?;
            if let Some(outcome) = receipt(client, &draft).await? {
                return Ok(outcome);
            }
            if generation != draft.binding.selection().generation
                || version != Some(draft.binding.selection().version.as_bytes().as_slice())
                || !draft.binding.roles().jmap()
            {
                return Err(Error::Policy);
            }
            if postgres_devices::authenticate(client, account_id, presented)
                .await
                .map_err(Error::Credential)?
                != Some(draft.device)
            {
                return Err(Error::Unauthenticated);
            }
            let old = sequence(state.try_get(0).map_err(|_| Error::Integrity)?)?;
            let floor = sequence(state.try_get(1).map_err(|_| Error::Integrity)?)?;
            if floor > old {
                return Err(Error::Integrity);
            }
            if draft.expected.is_some_and(|expected| expected != old) {
                return Err(Error::State);
            }
            let next = old.checked_add(1).ok_or(Error::Capacity)?;
            let count:i64=client.query_one("SELECT count(*) FROM (SELECT 1 FROM td_mta.draft_receipts WHERE account=$1 LIMIT $2) bounded", &[&account,&MAX_RECEIPTS]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
            if count >= MAX_RECEIPTS {
                return Err(Error::Capacity);
            }
            let mailbox = draft.mailbox.as_bytes().as_slice();
            let rows = client
                .query(
                    "SELECT changed FROM td_mta.mailboxes WHERE account=$1 AND id=$2 LIMIT 1",
                    &[&account, &mailbox],
                )
                .await
                .map_err(|_| Error::Database)?;
            let row = rows.first().ok_or(Error::MissingMailbox)?;
            if sequence(row.try_get(0).map_err(|_| Error::Integrity)?)? > old {
                return Err(Error::Integrity);
            }
            let email = draft.email.as_bytes().as_slice();
            let used:bool=client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.emails WHERE account=$1 AND id=$2) OR EXISTS(SELECT 1 FROM td_mta.changes WHERE account=$1 AND kind=3 AND object=$2)", &[&account,&email]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
            if used {
                return Err(Error::Conflict);
            }
            let (thread_id, new_thread) = publication::select_thread(
                client,
                account_id,
                &draft.anchors,
                draft.new_thread,
                old,
            )
            .await
            .map_err(publication_error)?;
            let thread = thread_id.as_bytes().as_slice();
            if new_thread {
                let used:bool=client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.threads WHERE account=$1 AND id=$2) OR EXISTS(SELECT 1 FROM td_mta.changes WHERE account=$1 AND kind=2 AND object=$2)", &[&account,&thread]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
                if used {
                    return Err(Error::Conflict);
                }
            }
            let usage = publication::charge(
                client,
                account_id,
                draft.binding.quotas(),
                [draft.bytes.len() as u64, 1, 0, 0, 0],
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
            publication::write_blob(
                client,
                publication::Blob {
                    account: account_id,
                    id: draft.blob,
                    bytes: &draft.bytes,
                    digest: &draft.body_digest,
                    created_at,
                    changed: next,
                },
                deadline,
            )
            .await
            .map_err(publication_error)?;
            let changed = next.to_be_bytes();
            let changed = changed.as_slice();
            let thread_query = if new_thread {
                "INSERT INTO td_mta.threads VALUES($1,$2,$3)"
            } else {
                "UPDATE td_mta.threads SET changed=$3 WHERE account=$1 AND id=$2"
            };
            if client
                .execute(thread_query, &[&account, &thread, &changed])
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            if client.execute("INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) VALUES($1,$2,$3,$4,$5,2,$6)", &[&account,&email,&draft.blob.as_bytes().as_slice(),&thread,&created_at,&changed]).await.map_err(|_|Error::Database)?!=1 { return Err(Error::Integrity); }
            if client
                .execute(
                    "INSERT INTO td_mta.memberships VALUES($1,$2,$3,$4)",
                    &[&account, &email, &mailbox, &changed],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            if client
                .execute(
                    "INSERT INTO td_mta.keywords VALUES($1,$2,'$draft',$3),($1,$2,'$seen',$3)",
                    &[&account, &email, &changed],
                )
                .await
                .map_err(|_| Error::Database)?
                != 2
            {
                return Err(Error::Integrity);
            }
            if client
                .execute(
                    "INSERT INTO td_mta.thread_anchors VALUES($1,$2,$3,$4)",
                    &[&account, &draft.message_id, &email, &changed],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            if client
                .execute(
                    "UPDATE td_mta.mailboxes SET changed=$3 WHERE account=$1 AND id=$2",
                    &[&account, &mailbox, &changed],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            let thread_action = if new_thread { 1i64 } else { 2 };
            if client.execute("INSERT INTO td_mta.changes VALUES($1,$2,0,3,1,$3),($1,$2,1,2,$6,$4),($1,$2,2,1,2,$5)", &[&account,&changed,&email,&thread,&mailbox,&thread_action]).await.map_err(|_|Error::Database)?!=3 { return Err(Error::Integrity); }
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
            let length = i64::try_from(draft.bytes.len()).map_err(|_| Error::Capacity)?;
            if client
                .execute(
                    "INSERT INTO td_mta.draft_receipts VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
                    &[
                        &account,
                        &draft.operation.as_bytes().as_slice(),
                        &draft.request_digest.as_slice(),
                        &email,
                        &draft.blob.as_bytes().as_slice(),
                        &thread,
                        &mailbox,
                        &length,
                        &created_at,
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
                email: draft.email,
                blob: draft.blob,
                thread: thread_id,
                mailbox: draft.mailbox,
                length: length as u64,
                created_at,
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
