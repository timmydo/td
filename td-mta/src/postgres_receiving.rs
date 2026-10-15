//! Retained owner-side SMTP publication; no listener or gateway admission.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    ids::{BlobId, EmailId, MailboxId, ReceivingOperationId, ThreadId},
    ports::{self, Clock, Deadline},
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
mod input;
pub use input::Message;
pub const MAX_RECEIPTS: i64 = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Input(crate::ports::Error),
    HeaderLimit,
    Epoch,
    Policy,
    Capacity,
    Quota,
    Integrity,
    Conflict,
    MissingMailbox,
    Crypto,
    Database,
    Indeterminate,
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
#[derive(Clone)]
pub struct Delivery {
    binding: Binding,
    operation: ReceivingOperationId,
    email: EmailId,
    blob: BlobId,
    new_thread: ThreadId,
    bytes: Arc<Vec<u8>>,
    body_digest: [u8; 32],
    request_digest: [u8; 32],
    message_id: String,
    anchors: Vec<String>,
    provenance: input::Provenance,
}
impl Delivery {
    pub fn operation(&self) -> ReceivingOperationId {
        self.operation
    }
    pub fn email(&self) -> EmailId {
        self.email
    }
    pub fn blob(&self) -> BlobId {
        self.blob
    }
    pub fn length(&self) -> u64 {
        self.bytes.len() as u64
    }
}
fn copy(value: &str) -> Result<String, Error> {
    let mut text = String::new();
    text.try_reserve_exact(value.len())
        .map_err(|_| Error::Capacity)?;
    text.push_str(value);
    Ok(text)
}
/// Prepare completed decoded DATA outside a database transaction. The trusted
/// caller retains connection/routing authority; this API does not admit peers.
pub fn prepare(
    binding: &Binding,
    message: &Message<'_>,
    deadline: Instant,
) -> Result<Delivery, Error> {
    preparation_time(binding, deadline)?;
    let clock = RuntimeClock::new();
    let left = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Error::Input(ports::Error::Deadline))?;
    let cap = Deadline::after(
        clock.sample().map_err(Error::Input)?.monotonic,
        u64::try_from(left.as_millis()).map_err(|_| Error::Capacity)?,
    )
    .map_err(Error::Input)?;
    let limits = binding.work();
    let mut meter = Meter::new(
        cap,
        Charge {
            io_bytes: limits.foreground_io_bytes,
            records: limits.foreground_records,
            output_bytes: binding.upload_maximum().min(32 * 1024 * 1024) as u64 + 1024 * 1024,
            ..Charge::default()
        },
    );
    prepare_with_meter(binding, message, deadline, &mut meter, &clock)
}

fn preparation_time(binding: &Binding, deadline: Instant) -> Result<(), Error> {
    if !binding.roles().receive() {
        return Err(Error::Policy);
    }
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero() && *left <= postgres_probe::MAX_DURATION)
        .ok_or(Error::Connection(postgres_probe::Error::Deadline))?;
    Ok(())
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
fn charge(clock: &impl Clock, meter: &mut Meter, bytes: usize, output: usize) -> Result<(), Error> {
    meter
        .charge(
            clock.sample().map_err(Error::Input)?.monotonic,
            Charge {
                io_bytes: bytes as u64,
                output_bytes: output as u64,
                records: 1,
                ..Charge::default()
            },
        )
        .map_err(|error| {
            Error::Input(match error {
                Stop::Deadline => ports::Error::Deadline,
                _ => ports::Error::Capacity,
            })
        })
}

/// Prepare completed DATA using its caller's original work allowance. Memory,
/// peer and routing admission remain the trusted connection owner's duty.
pub fn prepare_with_meter(
    binding: &Binding,
    message: &Message<'_>,
    deadline: Instant,
    meter: &mut Meter,
    clock: &RuntimeClock,
) -> Result<Delivery, Error> {
    preparation_time(binding, deadline)?;
    let clock = PreparationClock { clock, deadline };
    charge(&clock, meter, 0, 0)?;
    let mut ids = [[0; 16]; 4];
    let mut entropy = SystemEntropy::try_new().map_err(|_| Error::Crypto)?;
    for id in &mut ids {
        entropy.fill(id).map_err(|_| Error::Crypto)?;
    }
    let [operation, email, blob, new_thread] = ids;
    let (email, blob, new_thread) = (
        EmailId::from_bytes(email),
        BlobId::from_bytes(blob),
        ThreadId::from_bytes(new_thread),
    );
    let (bytes, anchors, message_id, provenance) =
        input::prepare(binding, message, email, blob, new_thread, &clock, meter)?;
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for part in bytes.chunks(publication::CHUNK) {
        charge(&clock, meter, part.len(), 0)?;
        hash.update(part).map_err(|_| Error::Crypto)?;
    }
    let body_digest = hash.finish().map_err(|_| Error::Crypto)?;
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for part in [
        b"td-mta PostgreSQL receiving v1\0".as_slice(),
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        &binding.selection().generation.to_be_bytes(),
        binding.selection().version.as_bytes().as_slice(),
        email.as_bytes().as_slice(),
        blob.as_bytes().as_slice(),
        new_thread.as_bytes().as_slice(),
        &(bytes.len() as u64).to_be_bytes(),
        &body_digest,
        &(provenance.encoded.len() as u64).to_be_bytes(),
        provenance.encoded.as_slice(),
        binding.profile().as_bytes(),
    ] {
        charge(&clock, meter, part.len(), 0)?;
        hash.update(part).map_err(|_| Error::Crypto)?;
    }
    let request_digest = hash.finish().map_err(|_| Error::Crypto)?;
    charge(&clock, meter, 0, 0)?;
    Ok(Delivery {
        binding: binding.clone(),
        operation: ReceivingOperationId::from_bytes(operation),
        email,
        blob,
        new_thread,
        bytes: Arc::new(bytes),
        body_digest,
        request_digest,
        message_id,
        anchors,
        provenance,
    })
}
fn sequence(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}

async fn begin(client: &Client, delivery: &Delivery) -> Result<(), Error> {
    let schema = postgres_schema::lock_and_inspect(client)
        .await
        .map_err(Error::Schema)?;
    if schema.store_epoch != delivery.binding.epoch() {
        return Err(Error::Epoch);
    }
    client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|Error::Database)
}
async fn receipt(client: &Client, delivery: &Delivery) -> Result<Option<Outcome>, Error> {
    let rows=client.query("SELECT request_digest,email_id,blob_id,thread_id,mailbox_id,length,created_at,old_sequence,new_sequence FROM td_mta.receiving_receipts WHERE account=$1 AND operation=$2 LIMIT 2", &[&delivery.binding.account().as_bytes().as_slice(),&delivery.operation.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 {
        return Err(Error::Integrity);
    }
    let row = rows.first().ok_or(Error::Integrity)?;
    let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if digest != delivery.request_digest {
        return Err(Error::Conflict);
    }
    let email: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
    let blob: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
    let thread: &[u8] = row.try_get(3).map_err(|_| Error::Integrity)?;
    let mailbox: &[u8] = row.try_get(4).map_err(|_| Error::Integrity)?;
    let length: i64 = row.try_get(5).map_err(|_| Error::Integrity)?;
    let old = sequence(row.try_get(7).map_err(|_| Error::Integrity)?)?;
    let next = sequence(row.try_get(8).map_err(|_| Error::Integrity)?)?;
    if email != delivery.email.as_bytes()
        || blob != delivery.blob.as_bytes()
        || usize::try_from(length).ok() != Some(delivery.bytes.len())
        || old.checked_add(1) != Some(next)
    {
        return Err(Error::Integrity);
    }
    Ok(Some(Outcome {
        email: delivery.email,
        blob: delivery.blob,
        thread: ThreadId::from_bytes(thread.try_into().map_err(|_| Error::Integrity)?),
        mailbox: MailboxId::from_bytes(mailbox.try_into().map_err(|_| Error::Integrity)?),
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
    delivery: &Delivery,
) -> Result<Option<Outcome>, Error> {
    let delivery = delivery.clone();
    let result = postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &delivery).await?;
            client
                .query_one(
                    "SELECT id FROM td_mta.accounts WHERE id=$1 FOR SHARE",
                    &[&delivery.binding.account().as_bytes().as_slice()],
                )
                .await
                .map_err(|_| Error::Database)?;
            receipt(client, &delivery).await
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
    delivery: &Delivery,
) -> Result<Outcome, Error> {
    if let Some(outcome) = inspect(request, trust, deadline, delivery)? {
        return Ok(outcome);
    }
    let delivery = delivery.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_upload_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &delivery).await?;
            let selected=client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR SHARE",&[]).await.map_err(|_|Error::Database)?;
            let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
            let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
            let account_id = delivery.binding.account();
            let account = account_id.as_bytes().as_slice();
            let state = client
                .query_one(
                    "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
                    &[&account],
                )
                .await
                .map_err(|_| Error::Database)?;
            if let Some(outcome) = receipt(client, &delivery).await? {
                return Ok(outcome);
            }
            if generation != delivery.binding.selection().generation
                || version != Some(delivery.binding.selection().version.as_bytes().as_slice())
                || !delivery.binding.roles().receive()
            {
                return Err(Error::Policy);
            }
            let old = sequence(state.try_get(0).map_err(|_| Error::Integrity)?)?;
            let floor = sequence(state.try_get(1).map_err(|_| Error::Integrity)?)?;
            if floor > old {
                return Err(Error::Integrity);
            }
            let next = old.checked_add(1).ok_or(Error::Capacity)?;
            let count:i64=client.query_one("SELECT count(*) FROM (SELECT 1 FROM td_mta.receiving_receipts WHERE account=$1 LIMIT $2) bounded", &[&account,&MAX_RECEIPTS]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
            if count >= MAX_RECEIPTS {
                return Err(Error::Capacity);
            }
            let inbox = client.query("SELECT id,changed FROM td_mta.mailboxes WHERE account=$1 AND role='inbox' ORDER BY id LIMIT 2", &[&account]).await.map_err(|_| Error::Database)?;
            if inbox.len() != 1 {
                return Err(Error::MissingMailbox);
            }
            let row = inbox.first().ok_or(Error::Integrity)?;
            let mailbox: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
            if mailbox.len() != 16 || sequence(row.try_get(1).map_err(|_| Error::Integrity)?)? > old
            {
                return Err(Error::Integrity);
            }
            let email = delivery.email.as_bytes().as_slice();
            let used:bool=client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.emails WHERE account=$1 AND id=$2) OR EXISTS(SELECT 1 FROM td_mta.changes WHERE account=$1 AND kind=3 AND object=$2)", &[&account,&email]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
            if used {
                return Err(Error::Conflict);
            }
            let (thread_id, new_thread) = publication::select_thread(
                client,
                account_id,
                &delivery.anchors,
                delivery.new_thread,
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
                delivery.binding.quotas(),
                [delivery.bytes.len() as u64, 1, 0, 0, 0],
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
                    id: delivery.blob,
                    bytes: &delivery.bytes,
                    digest: &delivery.body_digest,
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
            let facts = &delivery.provenance;
            let ipv4;
            let ipv6;
            let (family, peer): (i64, &[u8]) = match facts.peer {
                std::net::IpAddr::V4(ip) => {
                    ipv4 = ip.octets();
                    (4, &ipv4)
                }
                std::net::IpAddr::V6(ip) => {
                    ipv6 = ip.octets();
                    (6, &ipv6)
                }
            };
            let mut recipients = Vec::new();
            recipients
                .try_reserve_exact(facts.recipients.len())
                .map_err(|_| Error::Capacity)?;
            recipients.extend(facts.recipients.iter().map(String::as_str));
            let count = i64::try_from(recipients.len()).map_err(|_| Error::Capacity)?;
            if client.execute("INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,peer_family,peer_octets,gateway,tls,ehlo,reverse_path,receipt_count,changed) VALUES($1,$2,$3,$4,$5,1,$6,$7,$8,$9,$10,$11,$12,$13)", &[&account,&email,&delivery.blob.as_bytes().as_slice(),&thread,&facts.received_at,&family,&peer,&facts.gateway,&facts.tls,&facts.ehlo,&facts.reverse,&count,&changed]).await.map_err(|_| Error::Database)? != 1 { return Err(Error::Integrity); }
            if client.execute("INSERT INTO td_mta.smtp_receipt_recipients SELECT $1,$2,ordinality-1,address FROM unnest($3::text[]) WITH ORDINALITY q(address,ordinality)", &[&account,&email,&recipients]).await.map_err(|_| Error::Database)? != recipients.len() as u64 { return Err(Error::Integrity); }
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
            if !delivery.message_id.is_empty()
                && client
                    .execute(
                        "INSERT INTO td_mta.thread_anchors VALUES($1,$2,$3,$4)",
                        &[&account, &delivery.message_id, &email, &changed],
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
            let length = i64::try_from(delivery.bytes.len()).map_err(|_| Error::Capacity)?;
            if client
                .execute(
                    "INSERT INTO td_mta.receiving_receipts VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
                    &[
                        &account,
                        &delivery.operation.as_bytes().as_slice(),
                        &delivery.request_digest.as_slice(),
                        &email,
                        &delivery.blob.as_bytes().as_slice(),
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
                email: delivery.email,
                blob: delivery.blob,
                thread: thread_id,
                mailbox: MailboxId::from_bytes(mailbox.try_into().map_err(|_| Error::Integrity)?),
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
