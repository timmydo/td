//! Retained root-folder creation for the td-mail client.
use crate::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    device_credentials::Presented,
    ids::{DeviceId, MailboxId, MailboxOperationId},
    ports::{self, Clock, Deadline},
    postgres_devices,
    postgres_probe::{self, Request},
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

pub const MAX_CREATES: usize = 256;
pub const MAX_RECEIPTS: i64 = 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Credential(postgres_devices::Error),
    Validation(ports::Error),
    Epoch,
    Policy,
    Unauthenticated,
    Capacity,
    Integrity,
    Conflict,
    State,
    Crypto,
    Database,
    Indeterminate,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResultEntry {
    Created(MailboxId),
    InvalidName,
    AlreadyExists,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub old_sequence: u64,
    pub new_sequence: u64,
    pub results: Vec<ResultEntry>,
}
#[derive(Clone)]
struct Entry {
    name: String,
    id: MailboxId,
}
/// Keep the exact prepared operation until its commit outcome is resolved.
#[derive(Clone)]
pub struct Creation {
    binding: Binding,
    device: DeviceId,
    expected: Option<u64>,
    operation: MailboxOperationId,
    entries: Vec<Entry>,
    digest: [u8; 32],
}
impl Creation {
    pub fn operation(&self) -> MailboxOperationId {
        self.operation
    }
}

pub fn prepare(
    binding: &Binding,
    device: DeviceId,
    expected: Option<u64>,
    names: &[&str],
    deadline: Instant,
) -> Result<Creation, Error> {
    if !binding.roles().jmap() {
        return Err(Error::Policy);
    }
    if names.len() > MAX_CREATES {
        return Err(Error::Capacity);
    }
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero() && *left <= postgres_probe::MAX_DURATION)
        .ok_or(Error::Connection(postgres_probe::Error::Deadline))?;
    let clock = RuntimeClock::new();
    let cap = Deadline::after(
        clock.sample().map_err(Error::Validation)?.monotonic,
        u64::try_from(left.as_millis()).map_err(|_| Error::Capacity)?,
    )
    .map_err(Error::Validation)?;
    let work = binding.work();
    let mut meter = Meter::new(
        cap,
        Charge {
            io_bytes: work.foreground_io_bytes,
            records: work.foreground_records,
            output_bytes: MAX_CREATES as u64 * 1024,
            ..Charge::default()
        },
    );
    prepare_with_meter(
        binding, device, expected, names, deadline, &mut meter, &clock,
    )
}

/// Prepare using the HTTP request's original work allowance and clock.
pub fn prepare_with_meter(
    binding: &Binding,
    device: DeviceId,
    expected: Option<u64>,
    names: &[&str],
    deadline: Instant,
    meter: &mut Meter,
    clock: &RuntimeClock,
) -> Result<Creation, Error> {
    if !binding.roles().jmap() {
        return Err(Error::Policy);
    }
    if names.len() > MAX_CREATES {
        return Err(Error::Capacity);
    }
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    let mut scratch = td_mime::nfc::Scratch::new();
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(names.len())
        .map_err(|_| Error::Capacity)?;
    let mut entropy = SystemEntropy::try_new().map_err(|_| Error::Crypto)?;
    let mut operation = [0; 16];
    entropy.fill(&mut operation).map_err(|_| Error::Crypto)?;
    for name in names {
        if Instant::now() >= deadline {
            return Err(Error::Connection(postgres_probe::Error::Deadline));
        }
        let mut budget = td_mime::nfc::HeaderBudget::new();
        let valid = crate::mailbox_name::valid(name, meter, &mut budget, &mut scratch, || {
            Ok(clock.sample()?.monotonic)
        })
        .map_err(Error::Validation)?;
        let mut owned = String::new();
        if valid {
            owned
                .try_reserve_exact(name.len())
                .map_err(|_| Error::Capacity)?;
            owned.push_str(name);
        }
        let mut id = [0; 16];
        entropy.fill(&mut id).map_err(|_| Error::Crypto)?;
        entries.push(Entry {
            name: owned,
            id: MailboxId::from_bytes(id),
        });
    }
    let selected = binding.selection();
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for part in [
        b"td-mta PostgreSQL mailbox creation v1\0".as_slice(),
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        &selected.generation.to_be_bytes(),
        selected.version.as_bytes().as_slice(),
        device.as_bytes().as_slice(),
        &[u8::from(expected.is_some()), binding.roles().bits()],
        &expected.unwrap_or(0).to_be_bytes(),
        &(binding.profile().len() as u64).to_be_bytes(),
        binding.profile().as_bytes(),
        &(entries.len() as u64).to_be_bytes(),
    ] {
        hash.update(part).map_err(|_| Error::Crypto)?;
    }
    for entry in &entries {
        hash.update(&(entry.name.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(entry.name.as_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(entry.id.as_bytes())
            .map_err(|_| Error::Crypto)?;
    }
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    Ok(Creation {
        binding: binding.clone(),
        device,
        expected,
        operation: MailboxOperationId::from_bytes(operation),
        entries,
        digest: hash.finish().map_err(|_| Error::Crypto)?,
    })
}
fn sequence(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
async fn begin(client: &Client, creation: &Creation) -> Result<(), Error> {
    let schema = postgres_schema::lock_and_inspect(client)
        .await
        .map_err(Error::Schema)?;
    if schema.store_epoch != creation.binding.epoch() {
        return Err(Error::Epoch);
    }
    client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|Error::Database)
}
async fn receipt(client: &Client, creation: &Creation) -> Result<Option<Outcome>, Error> {
    let row=client.query_opt("SELECT request_digest,old_sequence,new_sequence,results FROM td_mta.mailbox_create_receipts WHERE account=$1 AND operation=$2", &[&creation.binding.account().as_bytes().as_slice(),&creation.operation.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if digest != creation.digest {
        return Err(Error::Conflict);
    }
    let old_sequence = sequence(row.try_get(1).map_err(|_| Error::Integrity)?)?;
    let new_sequence = sequence(row.try_get(2).map_err(|_| Error::Integrity)?)?;
    if new_sequence != old_sequence && Some(new_sequence) != old_sequence.checked_add(1) {
        return Err(Error::Integrity);
    }
    let bytes: &[u8] = row.try_get(3).map_err(|_| Error::Integrity)?;
    if bytes.len() != creation.entries.len() * 17 {
        return Err(Error::Integrity);
    }
    let mut results = Vec::new();
    results
        .try_reserve_exact(creation.entries.len())
        .map_err(|_| Error::Capacity)?;
    for (bytes, entry) in bytes.as_chunks::<17>().0.iter().zip(&creation.entries) {
        let id = bytes.get(1..).ok_or(Error::Integrity)?;
        results.push(match bytes.first() {
            Some(1) if id == entry.id.as_bytes() && !entry.name.is_empty() => {
                ResultEntry::Created(entry.id)
            }
            Some(2) if id == [0; 16] && entry.name.is_empty() => ResultEntry::InvalidName,
            Some(3) if id == [0; 16] && !entry.name.is_empty() => ResultEntry::AlreadyExists,
            _ => return Err(Error::Integrity),
        });
    }
    if results
        .iter()
        .any(|result| matches!(result, ResultEntry::Created(_)))
        != (new_sequence != old_sequence)
    {
        return Err(Error::Integrity);
    }
    Ok(Some(Outcome {
        old_sequence,
        new_sequence,
        results,
    }))
}
/// Owner-side receipt resolution waits behind a prior account publisher.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    creation: &Creation,
) -> Result<Option<Outcome>, Error> {
    let creation = creation.clone();
    let result = postgres_probe::with_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &creation).await?;
            client
                .query_one(
                    "SELECT id FROM td_mta.accounts WHERE id=$1 FOR SHARE",
                    &[&creation.binding.account().as_bytes().as_slice()],
                )
                .await
                .map_err(|_| Error::Database)?;
            receipt(client, &creation).await
        })
    })
    .map_err(Error::Connection)?;
    if !matches!(result, Ok(Some(_))) && Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}
/// Publication serializes policy, account and device authorization. Historical
/// success remains success after revocation; fresh writes require current state.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    creation: &Creation,
    password: &[u8],
) -> Result<Outcome, Error> {
    if let Some(outcome) = inspect(request, trust, deadline, creation)? {
        return Ok(outcome);
    }
    let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
    if presented.id() != creation.device {
        return Err(Error::Unauthenticated);
    }
    let creation = creation.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_batch_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &creation).await?;
            let selected=client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR SHARE", &[]).await.map_err(|_|Error::Database)?;
            let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
            let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
            let account = creation.binding.account();
            let account_bytes = account.as_bytes().as_slice();
            let state = client
                .query_one(
                    "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
                    &[&account_bytes],
                )
                .await
                .map_err(|_| Error::Database)?;
            if let Some(outcome) = receipt(client, &creation).await? {
                return Ok(outcome);
            }
            if generation != creation.binding.selection().generation
                || version != Some(creation.binding.selection().version.as_bytes().as_slice())
                || !creation.binding.roles().jmap()
            {
                return Err(Error::Policy);
            }
            if postgres_devices::authenticate(client, account, presented)
                .await
                .map_err(Error::Credential)?
                != Some(creation.device)
            {
                return Err(Error::Unauthenticated);
            }
            let old = sequence(state.try_get(0).map_err(|_| Error::Integrity)?)?;
            let floor = sequence(state.try_get(1).map_err(|_| Error::Integrity)?)?;
            if floor > old {
                return Err(Error::Integrity);
            }
            if creation.expected.is_some_and(|expected| expected != old) {
                return Err(Error::State);
            }
            let count:i64=client.query_one("SELECT count(*) FROM (SELECT 1 FROM td_mta.mailbox_create_receipts WHERE account=$1 LIMIT $2) bounded", &[&account_bytes,&MAX_RECEIPTS]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
            if count >= MAX_RECEIPTS {
                return Err(Error::Capacity);
            }
            let mut requested = Vec::new();
            requested
                .try_reserve_exact(creation.entries.len())
                .map_err(|_| Error::Capacity)?;
            for entry in &creation.entries {
                requested.push(entry.name.as_str());
            }
            let rows=client.query("SELECT name,changed FROM td_mta.mailboxes WHERE account=$1 AND parent_id IS NULL AND name=ANY($2::text[]) ORDER BY name LIMIT 257", &[&account_bytes,&requested]).await.map_err(|_|Error::Database)?;
            if rows.len() > MAX_CREATES {
                return Err(Error::Capacity);
            }
            let mut existing = Vec::new();
            existing
                .try_reserve_exact(rows.len())
                .map_err(|_| Error::Capacity)?;
            for row in &rows {
                let name: &str = row.try_get(0).map_err(|_| Error::Integrity)?;
                if sequence(row.try_get(1).map_err(|_| Error::Integrity)?)? > old
                    || existing.contains(&name)
                {
                    return Err(Error::Integrity);
                }
                existing.push(name);
            }
            let mut results = Vec::new();
            results
                .try_reserve_exact(creation.entries.len())
                .map_err(|_| Error::Capacity)?;
            let mut ids = Vec::new();
            ids.try_reserve_exact(creation.entries.len())
                .map_err(|_| Error::Capacity)?;
            let mut names = Vec::new();
            names
                .try_reserve_exact(creation.entries.len())
                .map_err(|_| Error::Capacity)?;
            for entry in &creation.entries {
                if entry.name.is_empty() {
                    results.push(ResultEntry::InvalidName);
                } else if existing.contains(&entry.name.as_str())
                    || names.contains(&entry.name.as_str())
                {
                    results.push(ResultEntry::AlreadyExists);
                } else {
                    if ids.contains(&entry.id.as_bytes().as_slice()) {
                        return Err(Error::Capacity);
                    }
                    ids.push(entry.id.as_bytes().as_slice());
                    names.push(entry.name.as_str());
                    results.push(ResultEntry::Created(entry.id));
                }
            }
            let mailbox_limit = i64::try_from(crate::postgres_mailboxes::MAX_MAILBOXES)
                .map_err(|_| Error::Capacity)?;
            let count: i64 = client
                .query_one(
                    "SELECT count(*) FROM (SELECT 1 FROM td_mta.mailboxes WHERE account=$1 LIMIT $2) bounded",
                    &[&account_bytes, &(mailbox_limit + 1)],
                )
                .await
                .map_err(|_| Error::Database)?
                .try_get(0)
                .map_err(|_| Error::Integrity)?;
            let count = usize::try_from(count).map_err(|_| Error::Integrity)?;
            if count.checked_add(ids.len()).ok_or(Error::Capacity)?
                > crate::postgres_mailboxes::MAX_MAILBOXES
            {
                return Err(Error::Capacity);
            }
            let next = if ids.is_empty() {
                old
            } else {
                crate::postgres_sequence::next(client, account_bytes, old)
                    .await
                    .map_err(|error| match error {
                        crate::postgres_sequence::Error::Capacity => Error::Capacity,
                        crate::postgres_sequence::Error::Integrity => Error::Integrity,
                        crate::postgres_sequence::Error::Database => Error::Database,
                    })?
            };
            if !ids.is_empty() {
                let collision:bool=client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.mailboxes WHERE account=$1 AND id=ANY($2::bytea[])) OR EXISTS(SELECT 1 FROM td_mta.changes WHERE account=$1 AND kind=1 AND object=ANY($2::bytea[]))", &[&account_bytes,&ids]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
                if collision {
                    return Err(Error::Capacity);
                }
                let changed = next.to_be_bytes();
                let written=client.execute("INSERT INTO td_mta.mailboxes(account,id,name,parent_id,role,sort_order,subscribed,changed) SELECT $1,id,name,NULL,NULL,0,1,$4 FROM unnest($2::bytea[],$3::text[]) q(id,name)", &[&account_bytes,&ids,&names,&changed.as_slice()]).await.map_err(|_|Error::Database)?;
                if written != ids.len() as u64 {
                    return Err(Error::Integrity);
                }
                let written=client.execute("INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) SELECT $1,$3,ordinality-1,1,1,id FROM unnest($2::bytea[]) WITH ORDINALITY q(id,ordinality)", &[&account_bytes,&ids,&changed.as_slice()]).await.map_err(|_|Error::Database)?;
                if written != ids.len() as u64 {
                    return Err(Error::Integrity);
                }
                if client
                    .execute(
                        "UPDATE td_mta.accounts SET sequence=$2 WHERE id=$1",
                        &[&account_bytes, &changed.as_slice()],
                    )
                    .await
                    .map_err(|_| Error::Database)?
                    != 1
                {
                    return Err(Error::Integrity);
                }
            }
            let mut encoded = Vec::new();
            encoded
                .try_reserve_exact(results.len() * 17)
                .map_err(|_| Error::Capacity)?;
            for result in &results {
                match result {
                    ResultEntry::Created(id) => {
                        encoded.push(1);
                        encoded.extend_from_slice(id.as_bytes());
                    }
                    ResultEntry::InvalidName => {
                        encoded.push(2);
                        encoded.extend_from_slice(&[0; 16]);
                    }
                    ResultEntry::AlreadyExists => {
                        encoded.push(3);
                        encoded.extend_from_slice(&[0; 16]);
                    }
                }
            }
            client.execute("INSERT INTO td_mta.mailbox_create_receipts(account,operation,request_digest,old_sequence,new_sequence,results) VALUES($1,$2,$3,$4,$5,$6)", &[&account_bytes,&creation.operation.as_bytes().as_slice(),&creation.digest.as_slice(),&old.to_be_bytes().as_slice(),&next.to_be_bytes().as_slice(),&encoded]).await.map_err(|_|Error::Database)?;
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(Outcome {
                old_sequence: old,
                new_sequence: next,
                results,
            })
        })
    });
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}
